#[cfg(test)]
mod signed_msg_order_id_eviction {
    use {
        crate::{
            error::ErrorCode,
            math::time::SlotClock,
            state::signed_msg_user::{
                SignedMsgOrderId, SignedMsgUserOrdersFixed, SignedMsgUserOrdersZeroCopyMut,
            },
        },
        anchor_lang::prelude::Pubkey,
        std::cell::RefCell,
    };

    #[test]
    fn signed_msg_order_id_exists() {
        let fixed = RefCell::new(SignedMsgUserOrdersFixed {
            user_pubkey: Pubkey::default(),
            padding: 0,
            len: 32,
        });
        let data = RefCell::new([0u8; 1280]);
        let mut signed_msg_user = SignedMsgUserOrdersZeroCopyMut {
            fixed: fixed.borrow_mut(),
            data: data.borrow_mut(),
        };

        let new_signed_msg_order_id = SignedMsgOrderId::new([7; 8], 10, 2);
        let add_result = signed_msg_user.add_signed_msg_order_id(new_signed_msg_order_id);
        assert!(add_result.is_ok());

        assert_eq!(
            signed_msg_user.check_exists_and_prune_stale_signed_msg_order_ids(
                new_signed_msg_order_id,
                11,
                SlotClock::baseline()
            ),
            true
        );
        assert_eq!(
            signed_msg_user.check_exists_and_prune_stale_signed_msg_order_ids(
                new_signed_msg_order_id,
                20,
                SlotClock::baseline()
            ),
            true
        );
        assert_eq!(
            signed_msg_user.check_exists_and_prune_stale_signed_msg_order_ids(
                new_signed_msg_order_id,
                30,
                SlotClock::baseline()
            ),
            false
        );

        let mut count = 0;
        for i in 0..32 {
            if signed_msg_user.get_mut(i).uuid == new_signed_msg_order_id.uuid {
                count += 1;
            }
        }
        assert_eq!(count, 0);
    }

    #[test]
    fn signed_msg_user_order_account_full() {
        let fixed = RefCell::new(SignedMsgUserOrdersFixed {
            user_pubkey: Pubkey::default(),
            padding: 0,
            len: 32,
        });

        let signed_msg_order_data: [SignedMsgOrderId; 32] =
            [SignedMsgOrderId::new([7; 8], 10, 1); 32];

        let mut byte_array = [0u8; 1280];
        for (i, order) in signed_msg_order_data.iter().enumerate() {
            let start = i * std::mem::size_of::<SignedMsgOrderId>();
            let end = start + std::mem::size_of::<SignedMsgOrderId>();
            byte_array[start..end].copy_from_slice(&borsh::to_vec(&order).unwrap());
        }

        let data = RefCell::new(byte_array);
        let mut signed_msg_user = SignedMsgUserOrdersZeroCopyMut {
            fixed: fixed.borrow_mut(),
            data: data.borrow_mut(),
        };

        let new_signed_msg_order_id = SignedMsgOrderId::new([7; 8], 10, 2);
        let add_result = signed_msg_user.add_signed_msg_order_id(new_signed_msg_order_id);
        assert!(add_result.is_err());
        assert_eq!(
            add_result.err().unwrap(),
            ErrorCode::SignedMsgUserOrdersAccountFull
        );
    }

    #[test]
    fn bad_signed_msg_order_ids() {
        let fixed = RefCell::new(SignedMsgUserOrdersFixed {
            user_pubkey: Pubkey::default(),
            padding: 0,
            len: 32,
        });

        let signed_msg_order_data: [SignedMsgOrderId; 32] =
            [SignedMsgOrderId::new([7; 8], 10, 1); 32];

        let mut byte_array = [0u8; 1280];
        for (i, order) in signed_msg_order_data.iter().enumerate() {
            let start = i * std::mem::size_of::<SignedMsgOrderId>();
            let end = start + std::mem::size_of::<SignedMsgOrderId>();
            byte_array[start..end].copy_from_slice(&borsh::to_vec(&order).unwrap());
        }

        let data = RefCell::new(byte_array);
        let mut signed_msg_user = SignedMsgUserOrdersZeroCopyMut {
            fixed: fixed.borrow_mut(),
            data: data.borrow_mut(),
        };

        let new_signed_msg_order_id = SignedMsgOrderId::new([7; 8], 10, 0);
        let add_result = signed_msg_user.add_signed_msg_order_id(new_signed_msg_order_id);
        assert!(add_result.is_err());
        assert_eq!(
            add_result.err().unwrap(),
            ErrorCode::InvalidSignedMsgOrderId
        );

        let new_signed_msg_order_id = SignedMsgOrderId::new([0; 8], 10, 10);
        let add_result = signed_msg_user.add_signed_msg_order_id(new_signed_msg_order_id);
        assert!(add_result.is_err());
        assert_eq!(
            add_result.err().unwrap(),
            ErrorCode::InvalidSignedMsgOrderId
        );

        let new_signed_msg_order_id = SignedMsgOrderId::new([7; 8], 0, 10);
        let add_result = signed_msg_user.add_signed_msg_order_id(new_signed_msg_order_id);
        assert!(add_result.is_err());
        assert_eq!(
            add_result.err().unwrap(),
            ErrorCode::InvalidSignedMsgOrderId
        );
    }
}

#[cfg(test)]
mod zero_copy {
    use {
        crate::{
            error::ErrorCode,
            state::signed_msg_user::{
                SignedMsgOrderId, SignedMsgUserOrders, SignedMsgUserOrdersLoader,
            },
            test_utils::create_account_info,
            ID,
        },
        anchor_lang::{prelude::Pubkey, Discriminator},
    };

    #[test]
    fn zero_copy() {
        let mut orders: SignedMsgUserOrders = SignedMsgUserOrders {
            authority_pubkey: Pubkey::default(),
            padding: 0,
            signed_msg_order_data: Vec::with_capacity(100),
        };

        for i in 0..100 {
            orders.signed_msg_order_data.push(SignedMsgOrderId {
                uuid: [0; 8],
                max_slot: 0,
                order_id: i as u32,
                padding: 0,
                clob_order_id: 0,
                route_digest: crate::state::order_params::NO_ROUTE_DIGEST,
            });
        }

        let mut bytes = Vec::with_capacity(8 + borsh::to_vec(&orders).unwrap().len());
        bytes.extend_from_slice(SignedMsgUserOrders::DISCRIMINATOR);
        bytes.extend_from_slice(&borsh::to_vec(&orders).unwrap());

        let pubkey = Pubkey::default();
        let mut lamports = 0;
        let orders_account_info =
            create_account_info(&pubkey, false, &mut lamports, &mut bytes, &ID);

        let orders_zero_copy = orders_account_info.load().unwrap();
        assert_eq!(orders_zero_copy.fixed.len, 100);
        for i in 0..100 {
            println!("i {}", i);
            assert_eq!(
                orders_zero_copy.get(i),
                &SignedMsgOrderId {
                    uuid: [0; 8],
                    max_slot: 0,
                    order_id: i,
                    padding: 0,
                    clob_order_id: 0,
                    route_digest: crate::state::order_params::NO_ROUTE_DIGEST,
                }
            );
        }

        drop(orders_zero_copy);

        // invalid owner
        let random_pubkey = Pubkey::new_unique();
        let orders_account_info = create_account_info(
            &random_pubkey,
            false,
            &mut lamports,
            &mut bytes,
            &random_pubkey,
        );
        let result = orders_account_info.load();
        assert!(result.is_err());
        assert_eq!(result.err().unwrap(), ErrorCode::DefaultError);

        // invalid discriminator
        let mut bytes = Vec::with_capacity(8 + borsh::to_vec(&orders).unwrap().len());
        bytes.extend_from_slice(&borsh::to_vec(&orders).unwrap());
        bytes.extend_from_slice(SignedMsgUserOrders::DISCRIMINATOR);
        let orders_account_info =
            create_account_info(&random_pubkey, false, &mut lamports, &mut bytes, &ID);
        let result = orders_account_info.load();
        assert!(result.is_err());
        assert_eq!(result.err().unwrap(), ErrorCode::DefaultError);
    }

    #[test]
    fn zero_copy_mut() {
        let mut orders: SignedMsgUserOrders = SignedMsgUserOrders {
            authority_pubkey: Pubkey::default(),
            padding: 0,
            signed_msg_order_data: Vec::with_capacity(100),
        };

        for i in 0..100 {
            orders.signed_msg_order_data.push(SignedMsgOrderId {
                uuid: [0; 8],
                max_slot: 0,
                order_id: i as u32,
                padding: 0,
                clob_order_id: 0,
                route_digest: crate::state::order_params::NO_ROUTE_DIGEST,
            });
        }

        let mut bytes = Vec::with_capacity(8 + borsh::to_vec(&orders).unwrap().len());
        bytes.extend_from_slice(SignedMsgUserOrders::DISCRIMINATOR);
        bytes.extend_from_slice(&borsh::to_vec(&orders).unwrap());

        let pubkey = Pubkey::default();
        let mut lamports = 0;
        let orders_account_info =
            create_account_info(&pubkey, true, &mut lamports, &mut bytes, &ID);

        let mut orders_zero_copy_mut = orders_account_info.load_mut().unwrap();

        assert_eq!(orders_zero_copy_mut.fixed.len, 100);
        for i in 0..100 {
            println!("i {}", i);
            assert_eq!(
                orders_zero_copy_mut.get_mut(i),
                &SignedMsgOrderId {
                    uuid: [0; 8],
                    max_slot: 0,
                    order_id: i,
                    padding: 0,
                    clob_order_id: 0,
                    route_digest: crate::state::order_params::NO_ROUTE_DIGEST,
                }
            );
        }

        drop(orders_zero_copy_mut);

        // invalid owner
        let random_pubkey = Pubkey::new_unique();
        let orders_account_info = create_account_info(
            &random_pubkey,
            true,
            &mut lamports,
            &mut bytes,
            &random_pubkey,
        );
        let result = orders_account_info.load_mut();
        assert!(result.is_err());
        assert_eq!(result.err().unwrap(), ErrorCode::DefaultError);

        // invalid discriminator
        let mut bytes = Vec::with_capacity(8 + borsh::to_vec(&orders).unwrap().len());
        bytes.extend_from_slice(&borsh::to_vec(&orders).unwrap());
        bytes.extend_from_slice(SignedMsgUserOrders::DISCRIMINATOR);
        let orders_account_info =
            create_account_info(&random_pubkey, true, &mut lamports, &mut bytes, &ID);
        let result = orders_account_info.load_mut();
        assert!(result.is_err());
        assert_eq!(result.err().unwrap(), ErrorCode::DefaultError);
    }
}

/// The entry outlives the message, because the order it became can still be
/// resting when the message's own window is long past.
#[cfg(test)]
mod resting_route {
    use {
        crate::{
            math::time::SlotClock,
            state::{
                order_params::NO_ROUTE_DIGEST,
                signed_msg_user::{
                    SignedMsgOrderId, SignedMsgUserOrdersFixed, SignedMsgUserOrdersZeroCopyMut,
                },
            },
        },
        anchor_lang::prelude::Pubkey,
        std::cell::RefCell,
    };

    const LEN: u32 = 4;
    const DIGEST: [u8; 8] = [9; 8];

    /// A stale sweep leaves an entry alone while its order rests, and takes it
    /// once the order is gone.
    #[test]
    fn a_resting_entry_survives_the_stale_sweep() {
        let fixed = RefCell::new(SignedMsgUserOrdersFixed {
            user_pubkey: Pubkey::default(),
            padding: 0,
            len: LEN,
        });
        let data = RefCell::new([0u8; 1280]);
        let mut orders = SignedMsgUserOrdersZeroCopyMut {
            fixed: fixed.borrow_mut(),
            data: data.borrow_mut(),
        };

        orders
            .add_signed_msg_order_id(SignedMsgOrderId::new([1; 8], 10, 1))
            .unwrap();
        assert!(orders.set_resting_route([1; 8], 77, DIGEST));

        // Far past the eviction buffer, but the order still rests.
        let probe = SignedMsgOrderId::new([2; 8], 10_000, 2);
        orders.check_exists_and_prune_stale_signed_msg_order_ids(
            probe,
            10_000,
            SlotClock::default(),
        );
        assert_eq!(orders.get(0).clob_order_id, 77);
        assert_eq!(orders.get(0).route_digest, DIGEST);

        // Once the order leaves the book the hold is released and the next
        // sweep reclaims the slot.
        assert!(orders.clear_resting_route(77));
        assert_eq!(orders.get(0).route_digest, NO_ROUTE_DIGEST);
        orders.check_exists_and_prune_stale_signed_msg_order_ids(
            probe,
            10_000,
            SlotClock::default(),
        );
        assert_eq!(orders.get(0), &SignedMsgOrderId::default());
    }

    /// A signed limit order carries no expiry, so retained entries could fill
    /// the account and stop the user trading. A full account reclaims the
    /// stalest of them instead of refusing.
    #[test]
    fn a_full_account_reclaims_the_stalest_resting_entry() {
        let fixed = RefCell::new(SignedMsgUserOrdersFixed {
            user_pubkey: Pubkey::default(),
            padding: 0,
            len: LEN,
        });
        let data = RefCell::new([0u8; 1280]);
        let mut orders = SignedMsgUserOrdersZeroCopyMut {
            fixed: fixed.borrow_mut(),
            data: data.borrow_mut(),
        };

        for i in 1..=LEN {
            let slot = u64::from(i) * 10;
            orders
                .add_signed_msg_order_id(SignedMsgOrderId::new([i as u8; 8], slot, i))
                .unwrap();
            assert!(orders.set_resting_route([i as u8; 8], u64::from(i), DIGEST));
        }

        // Every slot is held by a resting order, so the newcomer takes the one
        // whose message is oldest.
        orders
            .add_signed_msg_order_id(SignedMsgOrderId::new([0xEE; 8], 500, 99))
            .unwrap();
        assert_eq!(orders.get(0).uuid, [0xEE; 8]);
        assert_eq!(orders.get(0).clob_order_id, 0);
        // Clob order 1 lost its entry, so its fill reads as unrouted. Every
        // other order keeps its route.
        assert!(!orders.clear_resting_route(1));
        assert_eq!(orders.get(1).clob_order_id, 2);
        assert_eq!(orders.get(1).route_digest, DIGEST);
    }
}
