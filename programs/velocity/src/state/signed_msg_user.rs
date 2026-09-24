use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{
            safe_unwrap::SafeUnwrap,
            time::{Millis, SlotClock},
        },
        msg, validate, ID,
    },
    anchor_lang::{
        account,
        prelude::{
            borsh::{BorshDeserialize, BorshSerialize},
            Pubkey,
        },
        zero_copy, *,
    },
    prelude::AccountInfo,
    std::cell::{Ref, RefMut},
};

pub const SIGNED_MSG_PDA_SEED: &str = "SIGNED_MSG";
pub const SIGNED_MSG_WS_PDA_SEED: &str = "SIGNED_MSG_WS";
/// Grace past `max_slot` before a signed-message order id can be pruned. The
/// read site converts it to actual slots.
pub const SIGNED_MSG_EVICTION_BUFFER: Millis = Millis::from_ms(4_000);
/// How long a keeper may take to land a signed message. The order's worst
/// price was measured against the oracle at signing, so a message that lands
/// later than this no longer describes the market the signer agreed to.
pub const SIGNED_MSG_FILL_WINDOW: Millis = Millis::from_ms(30_000);

/// The last slot a keeper may place a signed message at. A resting limit's
/// message slot is itself the deadline, because the client stamps it ahead by
/// its signing budget. Any other order gets `SIGNED_MSG_FILL_WINDOW` past it.
pub fn signed_msg_max_slot(slot_clock: SlotClock, order_slot: u64, is_resting_limit: bool) -> u64 {
    if is_resting_limit {
        return order_slot;
    }

    slot_clock.slot_at_or_after_duration(order_slot, SIGNED_MSG_FILL_WINDOW)
}

mod tests;

/// One signed message this user sent, and what is still live from it.
///
/// The entry does two jobs. The `uuid` is replay protection, which is what
/// this account was built for. The rest is the routing state of the order the
/// message became. A signed-message order routes at placement and rests any
/// remainder on the market's CLOB. Somebody else builds the later transaction
/// that fills that remainder. `route_digest` holds that filler to the quoters
/// the taker chose, so it has to outlive the message.
///
/// The field order leaves no padding hole under `#[repr(C)]`. The two `u64`
/// fields sit on eight-byte boundaries, and the two byte arrays need no
/// alignment of their own. The stride is 40 bytes.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug, BorshDeserialize, BorshSerialize)]
#[repr(C)]
pub struct SignedMsgOrderId {
    pub uuid: [u8; 8],
    pub max_slot: u64,
    /// The CLOB order this message's remainder rests as, or zero when nothing
    /// of it rests. An entry naming a live order survives the stale sweep,
    /// because the fill that resolves it still needs the route below.
    pub clob_order_id: u64,
    pub order_id: u32,
    /// The market whose book `clob_order_id` names. Each book numbers its own
    /// orders, so the id alone can name an order on another market.
    pub market_index: u16,
    pub padding: u16,
    /// [`crate::state::order_params::route_digest`] of the quoter entries the
    /// taker's signed route named. Zero when the message named no route.
    pub route_digest: [u8; crate::state::order_params::ROUTE_DIGEST_LEN],
}

unsafe impl bytemuck::Pod for SignedMsgOrderId {}
unsafe impl bytemuck::Zeroable for SignedMsgOrderId {}

// The stride of the entry array, and therefore of every account this type is
// stored in. `SignedMsgUserOrders::space` derives the account size from it, so
// a change here changes what a client must allocate.
static_assertions::const_assert_eq!(std::mem::size_of::<SignedMsgOrderId>(), 40);

impl SignedMsgOrderId {
    pub fn new(uuid: [u8; 8], max_slot: u64, order_id: u32) -> Self {
        Self {
            uuid,
            max_slot,
            clob_order_id: 0,
            order_id,
            market_index: 0,
            padding: 0,
            route_digest: crate::state::order_params::NO_ROUTE_DIGEST,
        }
    }

    /// Whether this entry still describes an order resting on a book.
    pub fn rests_on_clob(&self) -> bool {
        self.clob_order_id != 0
    }

    fn rests_as(&self, market_index: u16, clob_order_id: u64) -> bool {
        clob_order_id != 0
            && self.clob_order_id == clob_order_id
            && self.market_index == market_index
    }
}

/**
 * This struct is a duplicate of SignedMsgUserOrdersZeroCopy
 * It is used to give anchor an struct to generate the idl for clients
 * The struct SignedMsgUserOrdersZeroCopy is used to load the data in efficiently
 */
#[account]
#[derive(Default, Eq, PartialEq, Debug)]
pub struct SignedMsgUserOrders {
    pub authority_pubkey: Pubkey,
    pub padding: u32,
    pub signed_msg_order_data: Vec<SignedMsgOrderId>,
}

impl SignedMsgUserOrders {
    /// 8 orders - 396 bytes - 0.00364704 SOL for rent
    /// 16 orders - 716 bytes - 0.00587424 SOL for rent
    /// 32 orders - 1356 bytes - 0.01032864 SOL for rent
    /// 64 orders - 2636 bytes - 0.01923744 SOL for rent
    pub fn space(num_orders: usize) -> usize {
        8 + 32 + 4 + 32 + num_orders * std::mem::size_of::<SignedMsgOrderId>()
    }

    pub fn validate(&self) -> VelocityResult<()> {
        validate!(
            !self.signed_msg_order_data.is_empty() && self.signed_msg_order_data.len() <= 128,
            ErrorCode::DefaultError,
            "SignedMsgUserOrders len must be between 1 and 128"
        )?;
        Ok(())
    }
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct SignedMsgUserOrdersFixed {
    pub user_pubkey: Pubkey,
    pub padding: u32,
    pub len: u32,
}

unsafe impl bytemuck::Pod for SignedMsgUserOrdersFixed {}
unsafe impl bytemuck::Zeroable for SignedMsgUserOrdersFixed {}

fn entry_at(data: &[u8], index: u32) -> &SignedMsgOrderId {
    let size = std::mem::size_of::<SignedMsgOrderId>();
    let start = index as usize * size;
    bytemuck::from_bytes(&data[start..start + size])
}

fn entry_at_mut(data: &mut [u8], index: u32) -> &mut SignedMsgOrderId {
    let size = std::mem::size_of::<SignedMsgOrderId>();
    let start = index as usize * size;
    bytemuck::from_bytes_mut(&mut data[start..start + size])
}

pub struct SignedMsgUserOrdersZeroCopy<'a> {
    pub fixed: Ref<'a, SignedMsgUserOrdersFixed>,
    pub data: Ref<'a, [u8]>,
}

impl<'a> SignedMsgUserOrdersZeroCopy<'a> {
    pub fn len(&self) -> u32 {
        self.fixed.len
    }

    pub fn get(&self, index: u32) -> &SignedMsgOrderId {
        entry_at(&self.data, index)
    }

    pub fn iter(&self) -> impl Iterator<Item = &SignedMsgOrderId> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }

    /// The route the signer chose for the order resting as `clob_order_id` on
    /// the book of `market_index`. `None` means the order carries no signed
    /// route: it was placed directly, or its entry was reclaimed.
    pub fn route_for_clob_order(
        &self,
        market_index: u16,
        clob_order_id: u64,
    ) -> Option<crate::state::order_params::RouteDigest> {
        self.iter()
            .find(|entry| entry.rests_as(market_index, clob_order_id))
            .map(|entry| entry.route_digest)
    }
}

pub struct SignedMsgUserOrdersZeroCopyMut<'a> {
    pub fixed: RefMut<'a, SignedMsgUserOrdersFixed>,
    pub data: RefMut<'a, [u8]>,
}

impl<'a> SignedMsgUserOrdersZeroCopyMut<'a> {
    pub fn len(&self) -> u32 {
        self.fixed.len
    }

    pub fn get(&self, index: u32) -> &SignedMsgOrderId {
        entry_at(&self.data, index)
    }

    pub fn get_mut(&mut self, index: u32) -> &mut SignedMsgOrderId {
        entry_at_mut(&mut self.data, index)
    }

    /// Replay check and stale sweep in one pass.
    ///
    /// An entry that names a live CLOB order is kept even once its `max_slot`
    /// is old. The remainder still rests, and the fill that resolves it reads
    /// the route from here. Only the pressure `add_signed_msg_order_id`
    /// describes reclaims such an entry, and only once it is past the buffer.
    ///
    /// This is the replay guard. It matches on the uuid, so an entry must
    /// outlive every slot at which its own message can still be placed.
    pub fn check_exists_and_prune_stale_signed_msg_order_ids(
        &mut self,
        signed_msg_order_id: SignedMsgOrderId,
        current_slot: u64,
        slot_clock: SlotClock,
    ) -> bool {
        let mut uuid_exists = false;
        for i in 0..self.len() {
            let existing_signed_msg_order_id = self.get_mut(i);
            let expired = slot_clock.elapsed(existing_signed_msg_order_id.max_slot, current_slot)
                > SIGNED_MSG_EVICTION_BUFFER;
            if existing_signed_msg_order_id.uuid == signed_msg_order_id.uuid && !expired {
                uuid_exists = true;
            } else if expired && !existing_signed_msg_order_id.rests_on_clob() {
                *existing_signed_msg_order_id = SignedMsgOrderId::default();
            }
        }
        uuid_exists
    }

    /// Take the free slot, or reclaim an expired retained one.
    ///
    /// A retained entry describes an order that may rest forever, because a
    /// signed limit order carries no expiry. Without a second pass those
    /// entries fill the account and the user can no longer trade. A full
    /// account therefore reclaims the entry whose `max_slot` is oldest, among
    /// the entries already past the eviction buffer.
    ///
    /// The expiry test is what makes the reclaim safe. An entry carries the
    /// uuid that `check_exists_and_prune_stale_signed_msg_order_ids` matches
    /// on, so reclaiming a live entry would re-admit its message and fill the
    /// same signed order a second time. An entry past `max_slot` plus the
    /// buffer cannot be re-admitted anyway, because placement refuses a
    /// message whose `max_slot` is behind the current slot. Releasing such an
    /// entry costs its resting order the route, and the fill then treats the
    /// order as unrouted. The taker's own limit price still bounds that fill.
    pub fn add_signed_msg_order_id(
        &mut self,
        signed_msg_order_id: SignedMsgOrderId,
        current_slot: u64,
        slot_clock: SlotClock,
    ) -> VelocityResult {
        if signed_msg_order_id.max_slot == 0
            || signed_msg_order_id.order_id == 0
            || signed_msg_order_id.uuid == [0; 8]
        {
            return Err(ErrorCode::InvalidSignedMsgOrderId);
        }

        for i in 0..self.len() {
            if self.get_mut(i).max_slot == 0 {
                *self.get_mut(i) = signed_msg_order_id;
                return Ok(());
            }
        }

        let stalest = (0..self.len())
            .filter(|i| {
                let entry = self.get(*i);
                entry.rests_on_clob()
                    && slot_clock.elapsed(entry.max_slot, current_slot) > SIGNED_MSG_EVICTION_BUFFER
            })
            .min_by_key(|i| self.get(*i).max_slot);
        match stalest {
            Some(index) => {
                msg!(
                    "signed msg order account full; reclaiming slot {} from clob order {}",
                    index,
                    self.get(index).clob_order_id
                );

                *self.get_mut(index) = signed_msg_order_id;
                Ok(())
            }
            None => Err(ErrorCode::SignedMsgUserOrdersAccountFull),
        }
    }

    /// Record that this message's remainder now rests on the book, with the
    /// route the fill that resolves it must carry.
    ///
    /// Returns false when the uuid is not held, which happens once the entry
    /// was reclaimed. The caller places the order either way. A remainder
    /// without a route is an unrouted order rather than a failed one.
    pub fn set_resting_route(
        &mut self,
        uuid: [u8; 8],
        market_index: u16,
        clob_order_id: u64,
        route_digest: crate::state::order_params::RouteDigest,
    ) -> bool {
        let Some(entry) = self.find_entry_mut(|entry| entry.uuid == uuid) else {
            return false;
        };

        entry.market_index = market_index;
        entry.clob_order_id = clob_order_id;
        entry.route_digest = route_digest;
        true
    }

    /// Point the entry of a replaced order at its replacement, so the route
    /// follows the order through a modify. The book gives the replacement a
    /// new id.
    pub fn move_resting_route(
        &mut self,
        market_index: u16,
        clob_order_id: u64,
        new_clob_order_id: u64,
    ) -> bool {
        let Some(entry) = self.find_entry_mut(|entry| entry.rests_as(market_index, clob_order_id))
        else {
            return false;
        };

        entry.clob_order_id = new_clob_order_id;
        true
    }

    fn find_entry_mut(
        &mut self,
        matches: impl Fn(&SignedMsgOrderId) -> bool,
    ) -> Option<&mut SignedMsgOrderId> {
        let index = (0..self.len()).find(|&i| matches(self.get(i)))?;

        Some(self.get_mut(index))
    }

    /// Release the entry's hold once its order leaves the book, so the stale
    /// sweep can reclaim the slot the ordinary way. Reclaim safety in
    /// `add_signed_msg_order_id` rests on the eviction buffer, not on this.
    pub fn clear_resting_route(&mut self, market_index: u16, clob_order_id: u64) -> bool {
        let Some(entry) = self.find_entry_mut(|entry| entry.rests_as(market_index, clob_order_id))
        else {
            return false;
        };

        entry.clob_order_id = 0;
        entry.route_digest = crate::state::order_params::NO_ROUTE_DIGEST;
        true
    }
}

pub trait SignedMsgUserOrdersLoader<'a> {
    fn load(&self) -> VelocityResult<SignedMsgUserOrdersZeroCopy<'_>>;
    fn load_mut(&self) -> VelocityResult<SignedMsgUserOrdersZeroCopyMut<'_>>;
}

impl<'a> SignedMsgUserOrdersLoader<'a> for AccountInfo<'a> {
    fn load(&self) -> VelocityResult<SignedMsgUserOrdersZeroCopy<'_>> {
        let owner = self.owner;

        validate!(
            owner == &ID,
            ErrorCode::DefaultError,
            "invalid signed_msg user orders owner",
        )?;

        let data = self.try_borrow_data().safe_unwrap()?;

        let (discriminator, data) = Ref::map_split(data, |d| d.split_at(8));
        validate!(
            discriminator.as_ref() == SignedMsgUserOrders::DISCRIMINATOR,
            ErrorCode::DefaultError,
            "invalid signed_msg user orders discriminator",
        )?;

        let (fixed, data) = Ref::map_split(data, |d| d.split_at(40));
        Ok(SignedMsgUserOrdersZeroCopy {
            fixed: Ref::map(fixed, |b| bytemuck::from_bytes(b)),
            data,
        })
    }

    fn load_mut(&self) -> VelocityResult<SignedMsgUserOrdersZeroCopyMut<'_>> {
        let owner = self.owner;

        validate!(
            owner == &ID,
            ErrorCode::DefaultError,
            "invalid signed_msg user orders owner",
        )?;

        let data = self.try_borrow_mut_data().safe_unwrap()?;

        let (discriminator, data) = RefMut::map_split(data, |d| d.split_at_mut(8));
        validate!(
            discriminator.as_ref() == SignedMsgUserOrders::DISCRIMINATOR,
            ErrorCode::DefaultError,
            "invalid signed_msg user orders discriminator",
        )?;

        let (fixed, data) = RefMut::map_split(data, |d| d.split_at_mut(40));
        Ok(SignedMsgUserOrdersZeroCopyMut {
            fixed: RefMut::map(fixed, |b| bytemuck::from_bytes_mut(b)),
            data,
        })
    }
}

/// The record of `authority`, when a removal path carries it as an optional
/// account. Any other account reads as absent, so a crank cannot fail on the
/// record of the user whose order it removes. A read-only record reads as
/// absent too, because a write to it fails the transaction.
pub fn carried_signed_msg_record<'a>(
    account: Option<&'a AccountInfo<'_>>,
    authority: &Pubkey,
) -> Option<SignedMsgUserOrdersZeroCopyMut<'a>> {
    let account = account.filter(|account| {
        account.owner == &ID
            && account.is_writable
            && account
                .try_borrow_data()
                .is_ok_and(|data| data.starts_with(SignedMsgUserOrders::DISCRIMINATOR))
    })?;
    let record = account.load_mut().ok()?;

    (record.fixed.user_pubkey == *authority).then_some(record)
}

/// Release the entries of the taker-origin remainders a removal took off the
/// book of `market_index`. The removals belong to one user, and `account` may
/// be that user's record.
pub fn release_removed_remainders(
    account: Option<&AccountInfo<'_>>,
    market_index: u16,
    removed: &[crate::state::prop_amm::RemovedOrderV0],
) {
    let mut remainders = removed.iter().filter(|order| order.taker_origin).peekable();
    let Some(owner) = remainders.peek().map(|order| order.user.authority) else {
        return;
    };

    let Some(mut record) = carried_signed_msg_record(account, &owner) else {
        return;
    };

    remainders.for_each(|order| {
        record.clear_resting_route(market_index, order.order_id);
    });
}

pub fn derive_signed_msg_user_pda(user_account_pubkey: &Pubkey) -> VelocityResult<Pubkey> {
    let (signed_msg_pubkey, _) = Pubkey::find_program_address(
        &[SIGNED_MSG_PDA_SEED.as_bytes(), user_account_pubkey.as_ref()],
        &ID,
    );
    Ok(signed_msg_pubkey)
}

/**
 * Used to store authenticated delegates for swift-like ws connections
 */
#[account]
#[derive(Default, Eq, PartialEq, Debug)]
pub struct SignedMsgWsDelegates {
    pub delegates: Vec<Pubkey>,
}

impl SignedMsgWsDelegates {
    pub fn space(&self, add: bool) -> usize {
        let delegate_count = if add {
            self.delegates.len() + 1
        } else {
            self.delegates.len() - 1
        };
        8 + 4 + delegate_count * 32
    }
}
