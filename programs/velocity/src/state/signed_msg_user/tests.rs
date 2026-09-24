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
        let add_result = signed_msg_user.add_signed_msg_order_id(
            new_signed_msg_order_id,
            11,
            SlotClock::baseline(),
        );

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
        let add_result = signed_msg_user.add_signed_msg_order_id(
            new_signed_msg_order_id,
            11,
            SlotClock::baseline(),
        );

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
        let add_result = signed_msg_user.add_signed_msg_order_id(
            new_signed_msg_order_id,
            11,
            SlotClock::baseline(),
        );

        assert!(add_result.is_err());
        assert_eq!(
            add_result.err().unwrap(),
            ErrorCode::InvalidSignedMsgOrderId
        );

        let new_signed_msg_order_id = SignedMsgOrderId::new([0; 8], 10, 10);
        let add_result = signed_msg_user.add_signed_msg_order_id(
            new_signed_msg_order_id,
            11,
            SlotClock::baseline(),
        );

        assert!(add_result.is_err());
        assert_eq!(
            add_result.err().unwrap(),
            ErrorCode::InvalidSignedMsgOrderId
        );

        let new_signed_msg_order_id = SignedMsgOrderId::new([7; 8], 0, 10);
        let add_result = signed_msg_user.add_signed_msg_order_id(
            new_signed_msg_order_id,
            11,
            SlotClock::baseline(),
        );

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
                market_index: 0,
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
                    market_index: 0,
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
                market_index: 0,
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
                    market_index: 0,
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
            error::ErrorCode,
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
    const MARKET: u16 = 3;

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

        let index = orders
            .add_signed_msg_order_id(
                SignedMsgOrderId::new([1; 8], 10, 1),
                10,
                SlotClock::default(),
            )
            .unwrap();
        orders.set_resting_route(index, MARKET, 77, DIGEST);

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
        assert!(orders.clear_resting_route(MARKET, 77));
        assert_eq!(orders.get(0).route_digest, NO_ROUTE_DIGEST);
        orders.check_exists_and_prune_stale_signed_msg_order_ids(
            probe,
            10_000,
            SlotClock::default(),
        );

        assert_eq!(orders.get(0), &SignedMsgOrderId::default());
    }

    /// A retained entry keeps its uuid after its message expires, so a later
    /// message can reuse the uuid. The route goes onto the entry just added.
    #[test]
    fn a_reused_uuid_routes_the_new_entry() {
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

        let old = orders
            .add_signed_msg_order_id(
                SignedMsgOrderId::new([1; 8], 10, 1),
                10,
                SlotClock::default(),
            )
            .unwrap();
        orders.set_resting_route(old, MARKET, 77, DIGEST);

        let reused = SignedMsgOrderId::new([1; 8], 20_000, 2);
        assert!(!orders.check_exists_and_prune_stale_signed_msg_order_ids(
            reused,
            10_000,
            SlotClock::default(),
        ));

        let new = orders
            .add_signed_msg_order_id(reused, 10_000, SlotClock::default())
            .unwrap();
        orders.set_resting_route(new, MARKET, 88, [3; 8]);

        assert_ne!(old, new);
        assert_eq!(orders.get(old).clob_order_id, 77);
        assert_eq!(orders.get(old).route_digest, DIGEST);
        assert_eq!(orders.get(new).clob_order_id, 88);
        assert_eq!(orders.get(new).route_digest, [3; 8]);
    }

    /// Fill every slot with a resting entry whose `max_slot` is `base_slot`
    /// plus ten times its index, so index 0 holds the oldest message.
    fn full_of_resting_entries(orders: &mut SignedMsgUserOrdersZeroCopyMut<'_>, base_slot: u64) {
        for i in 1..=LEN {
            let max_slot = base_slot + u64::from(i) * 10;
            let index = orders
                .add_signed_msg_order_id(
                    SignedMsgOrderId::new([i as u8; 8], max_slot, i),
                    base_slot,
                    SlotClock::default(),
                )
                .unwrap();
            orders.set_resting_route(index, MARKET, u64::from(i), DIGEST);
        }
    }

    /// Reclaiming a live entry would drop the uuid the replay guard matches
    /// on, and the message could then be placed a second time. A full account
    /// of live entries therefore refuses.
    #[test]
    fn a_full_account_refuses_while_every_entry_is_live() {
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

        full_of_resting_entries(&mut orders, 10_000);

        // Every entry's `max_slot` is ahead of the current slot, so every one
        // of them still guards a message that placement would accept.
        let result = orders.add_signed_msg_order_id(
            SignedMsgOrderId::new([0xEE; 8], 20_000, 99),
            10_000,
            SlotClock::default(),
        );

        assert_eq!(
            result.err().unwrap(),
            ErrorCode::SignedMsgUserOrdersAccountFull
        );

        // The first entry keeps its uuid and its route.
        assert_eq!(orders.get(0).uuid, [1; 8]);
        assert_eq!(orders.get(0).route_digest, DIGEST);
    }

    /// A signed limit order carries no expiry, so retained entries could fill
    /// the account and stop the user trading. The reclaim relieves that, but
    /// only for an entry past the eviction buffer. Such an entry's own message
    /// is already unplaceable, because placement refuses a `max_slot` behind
    /// the current slot.
    #[test]
    fn a_full_account_reclaims_only_an_expired_resting_entry() {
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

        full_of_resting_entries(&mut orders, 0);

        // The buffer is 4000ms, which is ten slots at the baseline duration.
        // At slot 30 only the first entry, whose `max_slot` is 10, is past it.
        let now = 30;
        orders
            .add_signed_msg_order_id(
                SignedMsgOrderId::new([0xEE; 8], 10_000, 99),
                now,
                SlotClock::default(),
            )
            .unwrap();
        assert_eq!(orders.get(0).uuid, [0xEE; 8]);
        assert_eq!(orders.get(0).clob_order_id, 0);
        // Clob order 1 lost its entry, so its fill reads as unrouted. Every
        // other order keeps its route.
        assert!(!orders.clear_resting_route(MARKET, 1));
        assert_eq!(orders.get(1).clob_order_id, 2);
        assert_eq!(orders.get(1).route_digest, DIGEST);

        // The entry the reclaim took was unplaceable: its `max_slot` was
        // behind the slot the add ran at.
        assert!(10 < now);
    }

    /// The second-oldest entry becomes reclaimable once it too passes the
    /// buffer, so the relief is not limited to one slot.
    #[test]
    fn the_reclaim_advances_as_entries_expire() {
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

        full_of_resting_entries(&mut orders, 0);

        // Slot 41 puts the first two entries, at `max_slot` 10 and 20, past
        // the ten-slot buffer. The reclaim takes the older of the two.
        let reclaimed = orders
            .add_signed_msg_order_id(
                SignedMsgOrderId::new([0xEE; 8], 10_000, 99),
                41,
                SlotClock::default(),
            )
            .unwrap();
        assert_eq!(reclaimed, 0);
        assert_eq!(orders.get(0).uuid, [0xEE; 8]);

        // The new entry rests nothing, so the next add takes it as a free
        // slot only after it is itself resting. Mark it, then reclaim again.
        orders.set_resting_route(reclaimed, MARKET, 99, DIGEST);
        orders
            .add_signed_msg_order_id(
                SignedMsgOrderId::new([0xEF; 8], 10_000, 100),
                41,
                SlotClock::default(),
            )
            .unwrap();
        assert_eq!(orders.get(1).uuid, [0xEF; 8]);
    }
}

/// Each book numbers its own orders, so an entry answers only for the market
/// it rests on.
#[cfg(test)]
mod market_scoped_route {
    use {
        crate::{
            math::time::SlotClock,
            state::{
                order_params::NO_ROUTE_DIGEST,
                signed_msg_user::{
                    carried_signed_msg_record, SignedMsgOrderId, SignedMsgUserOrders,
                    SignedMsgUserOrdersFixed, SignedMsgUserOrdersZeroCopy,
                    SignedMsgUserOrdersZeroCopyMut,
                },
            },
            test_utils::create_account_info,
            ID,
        },
        anchor_lang::{prelude::Pubkey, Discriminator},
        std::cell::RefCell,
    };

    const LEN: u32 = 4;
    const MARKET_A: u16 = 1;
    const MARKET_B: u16 = 2;
    const SHARED_ID: u64 = 5;
    const DIGEST_A: [u8; 8] = [0xA; 8];
    const DIGEST_B: [u8; 8] = [0xB; 8];

    struct Record {
        fixed: RefCell<SignedMsgUserOrdersFixed>,
        data: RefCell<[u8; 1280]>,
    }

    impl Record {
        fn new() -> Self {
            Self {
                fixed: RefCell::new(SignedMsgUserOrdersFixed {
                    user_pubkey: Pubkey::default(),
                    padding: 0,
                    len: LEN,
                }),
                data: RefCell::new([0u8; 1280]),
            }
        }

        fn orders_mut(&self) -> SignedMsgUserOrdersZeroCopyMut<'_> {
            SignedMsgUserOrdersZeroCopyMut {
                fixed: self.fixed.borrow_mut(),
                data: self.data.borrow_mut(),
            }
        }

        fn orders(&self) -> SignedMsgUserOrdersZeroCopy<'_> {
            SignedMsgUserOrdersZeroCopy {
                fixed: self.fixed.borrow(),
                data: self.data.borrow(),
            }
        }

        fn rest(&self, uuid: [u8; 8], market_index: u16, clob_order_id: u64, digest: [u8; 8]) {
            let mut orders = self.orders_mut();
            let index = orders
                .add_signed_msg_order_id(
                    SignedMsgOrderId::new(uuid, 10, u32::from(uuid[0])),
                    10,
                    SlotClock::default(),
                )
                .unwrap();
            orders.set_resting_route(index, market_index, clob_order_id, digest);
        }
    }

    /// The same id on another market is another order, and it carries no
    /// route of this entry's.
    #[test]
    fn a_route_answers_only_for_its_own_market() {
        let record = Record::new();
        record.rest([1; 8], MARKET_A, SHARED_ID, DIGEST_A);

        let orders = record.orders();
        assert_eq!(
            orders.route_for_clob_order(MARKET_A, SHARED_ID),
            Some(DIGEST_A)
        );
        assert_eq!(orders.route_for_clob_order(MARKET_B, SHARED_ID), None);
        assert_eq!(orders.route_for_clob_order(MARKET_A, 0), None);
    }

    /// Two remainders with one id on two books each keep their own route, and
    /// releasing one leaves the other resting.
    #[test]
    fn a_clear_releases_only_the_entry_on_its_market() {
        let record = Record::new();
        record.rest([1; 8], MARKET_A, SHARED_ID, DIGEST_A);
        record.rest([2; 8], MARKET_B, SHARED_ID, DIGEST_B);

        {
            let orders = record.orders();
            assert_eq!(
                orders.route_for_clob_order(MARKET_B, SHARED_ID),
                Some(DIGEST_B)
            );
        }

        let mut orders = record.orders_mut();
        assert!(orders.clear_resting_route(MARKET_B, SHARED_ID));
        assert!(!orders.clear_resting_route(MARKET_B, SHARED_ID));
        assert!(!orders.get(1).rests_on_clob());
        assert_eq!(orders.get(1).route_digest, NO_ROUTE_DIGEST);
        assert!(orders.get(0).rests_on_clob());
        assert_eq!(orders.get(0).route_digest, DIGEST_A);
    }

    /// A modify rests the order under a new id, and the route follows it.
    #[test]
    fn a_moved_route_follows_the_replacement_id() {
        let record = Record::new();
        record.rest([1; 8], MARKET_A, SHARED_ID, DIGEST_A);

        {
            let mut orders = record.orders_mut();
            assert!(!orders.move_resting_route(MARKET_B, SHARED_ID, 9));
            assert!(orders.move_resting_route(MARKET_A, SHARED_ID, 9));
        }

        let orders = record.orders();
        assert_eq!(orders.route_for_clob_order(MARKET_A, 9), Some(DIGEST_A));
        assert_eq!(orders.route_for_clob_order(MARKET_A, SHARED_ID), None);
    }

    fn record_bytes(authority: &Pubkey) -> Vec<u8> {
        let orders = SignedMsgUserOrders {
            authority_pubkey: *authority,
            padding: 0,
            signed_msg_order_data: vec![SignedMsgOrderId::default(); LEN as usize],
        };
        let mut bytes = SignedMsgUserOrders::DISCRIMINATOR.to_vec();
        bytes.extend_from_slice(&borsh::to_vec(&orders).unwrap());
        bytes
    }

    /// A removal path reads anything but the owner's writable record as
    /// absent, so it never fails on the account.
    #[test]
    fn only_the_owners_writable_record_is_carried() {
        let authority = Pubkey::new_unique();
        let key = Pubkey::new_unique();
        let mut lamports = 0;

        assert!(carried_signed_msg_record(None, &authority).is_none());

        let mut bytes = record_bytes(&authority);
        let read_only = create_account_info(&key, false, &mut lamports, &mut bytes, &ID);
        assert!(carried_signed_msg_record(Some(&read_only), &authority).is_none());

        let mut bytes = record_bytes(&authority);
        let system = Pubkey::default();
        let foreign_owner = create_account_info(&key, true, &mut lamports, &mut bytes, &system);
        assert!(carried_signed_msg_record(Some(&foreign_owner), &authority).is_none());

        let mut bytes = record_bytes(&Pubkey::new_unique());
        let other_user = create_account_info(&key, true, &mut lamports, &mut bytes, &ID);
        assert!(carried_signed_msg_record(Some(&other_user), &authority).is_none());

        let mut bytes = vec![0u8; 8 + 40];
        let wrong_type = create_account_info(&key, true, &mut lamports, &mut bytes, &ID);
        assert!(carried_signed_msg_record(Some(&wrong_type), &authority).is_none());

        let mut bytes = record_bytes(&authority);
        let owners = create_account_info(&key, true, &mut lamports, &mut bytes, &ID);
        assert!(carried_signed_msg_record(Some(&owners), &authority).is_some());
    }
}

#[cfg(test)]
mod signed_msg_max_slot {
    use crate::{
        math::time::SlotClock,
        state::signed_msg_user::{signed_msg_max_slot, SIGNED_MSG_FILL_WINDOW},
    };

    #[test]
    fn a_resting_limit_is_placeable_only_until_its_stamp() {
        assert_eq!(signed_msg_max_slot(SlotClock::baseline(), 100, true), 100);
    }

    #[test]
    fn a_taker_order_gets_the_fill_window_past_its_stamp() {
        let clock = SlotClock::baseline();
        assert_eq!(
            signed_msg_max_slot(clock, 100, false),
            clock.slot_at_or_after_duration(100, SIGNED_MSG_FILL_WINDOW)
        );
        assert!(signed_msg_max_slot(clock, 100, false) > 100);
    }
}
