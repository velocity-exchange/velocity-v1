//! Collateral reservations for txs that take on a position
//!
//! Each liquidator subaccount keeps its free collateral as last observed on chain, with the
//! slot of that snapshot, and every reservation against it. Available collateral is never
//! stored: it is the chain value less the reservations that snapshot does not reflect yet, so a
//! release cannot create collateral the chain does not have. A route reserves with
//! `try_reserve` before its tx is sent, under the subaccount's lock, so two concurrent
//! liquidations cannot commit the same collateral. The returned guard releases the reservation
//! if it is dropped before the tx reaches the tx worker. From then on the worker settles it with
//! the slot the tx confirmed at, or releases it when the tx is rejected or fails. A settled
//! reservation counts until a snapshot from a later slot arrives: an account write from the
//! confirmation slot itself may precede the tx. Nothing is released on time alone. The
//! liquidator reconciles a reservation with no outcome against its tx's status and blockhash
//! (`unresolved`), and a settled one with no newer snapshot against an RPC read of the account
//! (`awaiting_snapshot`).

use {
    crate::common::keeper::unix_now_ms,
    dashmap::DashMap,
    solana_signature::Signature,
    std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    velocity_rs::{types::solana_sdk::message::Hash, Pubkey},
};

/// A reservation with no outcome this long after it was taken is reconciled against its tx.
pub const RESERVATION_OUTCOME_TIMEOUT_MS: u64 = 60_000;

pub type ReservationId = u64;

#[derive(Clone, Copy, Debug)]
struct Balance {
    /// Free collateral in the snapshot, which can be negative.
    chain_free: i128,
    /// The slot the snapshot's account data is from.
    snapshot_slot: u64,
}

#[derive(Clone, Copy, Debug)]
struct Reservation {
    subaccount: Pubkey,
    amount: u128,
    reserved_ms: u64,
    /// The slot the tx confirmed at, once it has.
    confirmed_slot: Option<u64>,
    /// The signed tx's signature and blockhash, once signed.
    tx: Option<(Signature, Hash)>,
}

impl Reservation {
    /// A snapshot from `snapshot_slot` includes this reservation's tx. It must come from a later
    /// slot than the confirmation: an account write from the same slot may precede the tx.
    fn reflected_in(&self, snapshot_slot: u64) -> bool {
        self.confirmed_slot
            .is_some_and(|confirmed_slot| confirmed_slot < snapshot_slot)
    }
}

#[derive(Clone, Default)]
pub struct CollateralBook {
    balances: Arc<DashMap<Pubkey, Balance>>,
    reservations: Arc<DashMap<ReservationId, Reservation>>,
    next_id: Arc<AtomicU64>,
}

impl CollateralBook {
    /// Free collateral not yet committed by an in-flight tx. `None` before the first snapshot.
    pub fn available(&self, subaccount: &Pubkey) -> Option<u128> {
        let balance = self.balances.get(subaccount)?;
        Some(self.available_in(subaccount, &balance))
    }

    /// The largest available collateral among `subaccounts`, 0 when none is known.
    pub fn max_available(&self, subaccounts: &[Pubkey]) -> u128 {
        subaccounts
            .iter()
            .filter_map(|subaccount| self.available(subaccount))
            .max()
            .unwrap_or(0)
    }

    fn available_in(&self, subaccount: &Pubkey, balance: &Balance) -> u128 {
        let outstanding: u128 = self
            .reservations
            .iter()
            .filter(|reservation| {
                reservation.subaccount == *subaccount
                    && !reservation.reflected_in(balance.snapshot_slot)
            })
            .map(|reservation| reservation.amount)
            .sum();
        balance
            .chain_free
            .saturating_sub(outstanding.min(i128::MAX as u128) as i128)
            .max(0) as u128
    }

    /// Reserve `amount` of `subaccount`'s available collateral. `None` when it has less, which
    /// includes another task having reserved it first.
    pub fn try_reserve(&self, subaccount: Pubkey, amount: u128) -> Option<ReservationGuard> {
        // the subaccount's lock makes the check and the insert one step
        let balance = self.balances.get_mut(&subaccount)?;
        if self.available_in(&subaccount, &balance) < amount {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.reservations.insert(
            id,
            Reservation {
                subaccount,
                amount,
                reserved_ms: unix_now_ms(),
                confirmed_slot: None,
                tx: None,
            },
        );
        drop(balance);
        Some(ReservationGuard {
            book: self.clone(),
            id,
            armed: true,
        })
    }

    /// The tx was rejected or failed: the collateral is available again.
    pub fn release(&self, id: ReservationId) {
        self.reservations.remove(&id);
    }

    /// The tx confirmed at `confirmed_slot`. The reservation keeps counting until a snapshot
    /// from that slot or later includes the tx.
    pub fn settle(&self, id: ReservationId, confirmed_slot: u64) {
        if let Some(mut reservation) = self.reservations.get_mut(&id) {
            reservation.confirmed_slot = Some(confirmed_slot);
        }
    }

    /// Record a chain snapshot of `subaccount`'s free collateral taken at `snapshot_slot`. An
    /// older snapshot than the one held is ignored. Reservations the snapshot reflects are
    /// dropped.
    pub fn observe(&self, subaccount: Pubkey, chain_free: i128, snapshot_slot: u64) {
        let mut balance = self.balances.entry(subaccount).or_insert(Balance {
            chain_free,
            snapshot_slot,
        });
        if snapshot_slot < balance.snapshot_slot {
            return;
        }
        *balance = Balance {
            chain_free,
            snapshot_slot,
        };
        self.reservations.retain(|_, reservation| {
            reservation.subaccount != subaccount || !reservation.reflected_in(snapshot_slot)
        });
    }

    /// Record the signed tx that holds reservation `id`. False when the reservation is gone, in
    /// which case the tx must not be sent: it would spend collateral nobody reserved.
    pub fn attach(&self, id: ReservationId, signature: Signature, blockhash: Hash) -> bool {
        let Some(mut reservation) = self.reservations.get_mut(&id) else {
            return false;
        };
        reservation.tx = Some((signature, blockhash));
        true
    }

    /// Reservations still without an outcome `RESERVATION_OUTCOME_TIMEOUT_MS` after they were
    /// taken, with their signed tx, to reconcile against the tx's status. A reservation without
    /// a tx is left out: its `ReservationGuard` is still with a live sender, and the guard
    /// releases it if the send never happens.
    pub fn unresolved(&self) -> Vec<(ReservationId, Signature, Hash)> {
        let now = unix_now_ms();
        self.reservations
            .iter()
            .filter(|entry| {
                entry.confirmed_slot.is_none()
                    && now.saturating_sub(entry.reserved_ms) > RESERVATION_OUTCOME_TIMEOUT_MS
            })
            .filter_map(|entry| {
                let (signature, blockhash) = entry.tx?;
                Some((*entry.key(), signature, blockhash))
            })
            .collect()
    }
    /// Subaccounts holding a settled reservation that their snapshot does not include yet, with
    /// the latest confirmation slot among them. A snapshot from a later slot resolves them.
    pub fn awaiting_snapshot(&self) -> Vec<(Pubkey, u64)> {
        // Copy the settled reservations out and drop every reservations guard before reading
        // balances: `try_reserve` and `observe` hold a balances lock while they take
        // reservation locks, so holding both here in the other order could deadlock.
        let settled: Vec<(Pubkey, u64)> = self
            .reservations
            .iter()
            .filter_map(|entry| Some((entry.subaccount, entry.confirmed_slot?)))
            .collect();

        let mut awaiting = std::collections::BTreeMap::<Pubkey, u64>::new();
        for (subaccount, confirmed_slot) in settled {
            let snapshot_slot = self
                .balances
                .get(&subaccount)
                .map_or(0, |balance| balance.snapshot_slot);
            if confirmed_slot >= snapshot_slot {
                let latest = awaiting.entry(subaccount).or_insert(confirmed_slot);
                *latest = (*latest).max(confirmed_slot);
            }
        }
        awaiting.into_iter().collect()
    }
}

/// A reservation that releases itself when dropped, until `hand_off` gives it to the tx worker.
/// A send that is cancelled or fails before the worker has the tx then frees the collateral at
/// once.
#[must_use]
pub struct ReservationGuard {
    book: CollateralBook,
    id: ReservationId,
    armed: bool,
}

impl ReservationGuard {
    pub fn id(&self) -> ReservationId {
        self.id
    }

    /// Record the signed tx that will hold this reservation. False when the reservation is
    /// gone, so the tx must not be sent.
    #[must_use]
    pub fn attach(&self, signature: Signature, blockhash: Hash) -> bool {
        self.book.attach(self.id, signature, blockhash)
    }

    /// The tx worker now owns the outcome.
    pub fn hand_off(mut self) -> ReservationId {
        self.armed = false;
        self.id
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.book.release(self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{CollateralBook, RESERVATION_OUTCOME_TIMEOUT_MS},
        crate::common::keeper::unix_now_ms,
        solana_signature::Signature,
        velocity_rs::{types::solana_sdk::message::Hash, Pubkey},
    };

    fn book(subaccount: Pubkey, chain_free: i128) -> CollateralBook {
        let book = CollateralBook::default();
        book.observe(subaccount, chain_free, 100);
        book
    }

    #[test]
    fn reservation_cannot_overbook() {
        let subaccount = Pubkey::new_unique();
        let book = book(subaccount, 1_000);

        let first = book.try_reserve(subaccount, 700).expect("fits").hand_off();
        assert_eq!(book.available(&subaccount), Some(300));
        // a second liquidation cannot take the same collateral
        assert!(book.try_reserve(subaccount, 700).is_none());

        book.release(first);
        assert_eq!(book.available(&subaccount), Some(1_000));
        // releasing twice frees it once
        book.release(first);
        assert_eq!(book.available(&subaccount), Some(1_000));
    }

    #[test]
    fn release_after_a_lower_snapshot_creates_no_collateral() {
        // regression: free was stored clamped, so a release after a refresh that clamped it
        // to 0 credited back collateral the chain did not have
        let subaccount = Pubkey::new_unique();
        let book = book(subaccount, 1_000);

        let id = book.try_reserve(subaccount, 700).expect("fits").hand_off();
        book.observe(subaccount, 200, 101);
        assert_eq!(book.available(&subaccount), Some(0));

        book.release(id);
        assert_eq!(book.available(&subaccount), Some(200));
    }

    #[test]
    fn settled_reservation_counts_until_a_snapshot_includes_it() {
        let subaccount = Pubkey::new_unique();
        let book = book(subaccount, 1_000);
        let id = book.try_reserve(subaccount, 400).expect("fits").hand_off();

        // the tx confirms at slot 105, but the latest snapshot is from before it
        book.settle(id, 105);
        assert_eq!(book.available(&subaccount), Some(600));
        // a snapshot from before the tx arrives late: still counted
        book.observe(subaccount, 1_000, 104);
        assert_eq!(book.available(&subaccount), Some(600));
        // a snapshot from a later slot includes the tx and replaces the reservation
        book.observe(subaccount, 600, 106);
        assert_eq!(book.available(&subaccount), Some(600));
    }

    #[test]
    fn snapshot_that_already_includes_the_tx_is_right_once_settled() {
        let subaccount = Pubkey::new_unique();
        let book = book(subaccount, 1_000);
        let id = book.try_reserve(subaccount, 400).expect("fits").hand_off();

        // the post-tx snapshot arrives before the confirmation: counted twice for now
        book.observe(subaccount, 600, 110);
        assert_eq!(book.available(&subaccount), Some(200));
        // the confirmation at slot 105 shows the snapshot includes the tx
        book.settle(id, 105);
        assert_eq!(book.available(&subaccount), Some(600));
    }

    #[test]
    fn older_snapshot_is_ignored() {
        let subaccount = Pubkey::new_unique();
        let book = book(subaccount, 1_000);
        book.observe(subaccount, 5_000, 99);
        assert_eq!(book.available(&subaccount), Some(1_000));
    }

    #[test]
    fn dropped_guard_releases_before_hand_off() {
        let subaccount = Pubkey::new_unique();
        let book = book(subaccount, 1_000);
        {
            let _guard = book.try_reserve(subaccount, 700).expect("fits");
            assert_eq!(book.available(&subaccount), Some(300));
        }
        assert_eq!(book.available(&subaccount), Some(1_000));
    }

    #[test]
    fn snapshot_from_the_confirmation_slot_does_not_count_as_including_the_tx() {
        // an account write from the same slot may precede the tx
        let subaccount = Pubkey::new_unique();
        let book = CollateralBook::default();
        book.observe(subaccount, 1_000, 105);
        let id = book.try_reserve(subaccount, 400).expect("fits").hand_off();
        book.settle(id, 105);
        assert_eq!(book.available(&subaccount), Some(600));
        assert_eq!(book.awaiting_snapshot(), vec![(subaccount, 105)]);

        book.observe(subaccount, 600, 106);
        assert_eq!(book.available(&subaccount), Some(600));
        assert!(book.awaiting_snapshot().is_empty());
    }

    #[test]
    fn settled_reservation_is_never_released_on_time() {
        let subaccount = Pubkey::new_unique();
        let book = book(subaccount, 1_000);
        let id = book.try_reserve(subaccount, 400).expect("fits").hand_off();
        book.settle(id, 105);
        book.reservations.get_mut(&id).unwrap().reserved_ms =
            unix_now_ms() - RESERVATION_OUTCOME_TIMEOUT_MS - 1;

        assert!(book.unresolved().is_empty());
        assert_eq!(book.available(&subaccount), Some(600));
    }

    #[test]
    fn old_reservation_without_an_outcome_is_reconciled_not_released() {
        let subaccount = Pubkey::new_unique();
        let book = book(subaccount, 1_000);
        let signed = book.try_reserve(subaccount, 300).expect("fits");
        assert!(signed.attach(Signature::from([1u8; 64]), Hash::default()));
        let signed = signed.hand_off();
        let unsigned = book.try_reserve(subaccount, 300).expect("fits").hand_off();
        for id in [signed, unsigned] {
            book.reservations.get_mut(&id).unwrap().reserved_ms =
                unix_now_ms() - RESERVATION_OUTCOME_TIMEOUT_MS - 1;
        }

        // the signed tx goes to reconciliation, and the unsigned one stays with its guard's
        // owner; both keep their collateral
        let unresolved = book.unresolved();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].0, signed);
        assert_eq!(book.available(&subaccount), Some(400));
        assert!(book.attach(unsigned, Signature::from([2u8; 64]), Hash::default()));
    }

    #[test]
    fn concurrent_reads_and_writes_do_not_deadlock() {
        // regression: `awaiting_snapshot` held reservation guards while taking balance locks,
        // the reverse of `observe` and `try_reserve`
        use std::{sync::mpsc, thread, time::Duration};

        let subaccounts: Vec<Pubkey> = (0..4).map(|_| Pubkey::new_unique()).collect();
        let book = CollateralBook::default();
        for subaccount in &subaccounts {
            book.observe(*subaccount, 1_000_000, 1);
        }

        let (done, finished) = mpsc::channel();
        for worker in 0..6usize {
            let book = book.clone();
            let subaccounts = subaccounts.clone();
            let done = done.clone();
            thread::spawn(move || {
                for round in 0..2_000u64 {
                    let subaccount = subaccounts[(worker + round as usize) % subaccounts.len()];
                    match worker % 3 {
                        0 => {
                            if let Some(guard) = book.try_reserve(subaccount, 1) {
                                let id = guard.hand_off();
                                book.settle(id, round);
                            }
                        }
                        1 => book.observe(subaccount, 1_000_000, round + 2),
                        _ => {
                            let _ = book.awaiting_snapshot();
                            let _ = book.available(&subaccount);
                        }
                    }
                }
                done.send(()).unwrap();
            });
        }
        for _ in 0..6 {
            finished
                .recv_timeout(Duration::from_secs(30))
                .expect("a worker deadlocked");
        }
    }
}
