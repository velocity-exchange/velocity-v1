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
/// Grace past `max_slot` before a signed-msg order id is prunable (~4s),
/// expressed in actual slots at the read site.
pub const SIGNED_MSG_EVICTION_BUFFER: Millis = Millis::from_ms(4_000);

mod tests;

/// One signed message this user sent, and what is still live from it.
///
/// Two jobs. The `uuid` is replay protection, which is what this account was
/// built for. The rest is the routing state of the order that message became:
/// a signed-message order routes at placement and rests any remainder on the
/// market's CLOB, and the fill that resolves that remainder happens in a later
/// transaction built by somebody else. `route_digest` is what holds that
/// somebody to the quoters the taker chose, so it has to outlive the message.
///
/// Field order is chosen so `#[repr(C)]` leaves no padding hole: the two `u64`
/// fields sit on eight-byte boundaries and the two byte arrays need no
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
    pub padding: u32,
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
            padding: 0,
            route_digest: crate::state::order_params::NO_ROUTE_DIGEST,
        }
    }

    /// Whether this entry still describes an order resting on a book.
    pub fn rests_on_clob(&self) -> bool {
        self.clob_order_id != 0
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

pub struct SignedMsgUserOrdersZeroCopy<'a> {
    pub fixed: Ref<'a, SignedMsgUserOrdersFixed>,
    pub data: Ref<'a, [u8]>,
}

impl<'a> SignedMsgUserOrdersZeroCopy<'a> {
    pub fn len(&self) -> u32 {
        self.fixed.len
    }

    pub fn get(&self, index: u32) -> &SignedMsgOrderId {
        let size = std::mem::size_of::<SignedMsgOrderId>();
        let start = index as usize * size;
        bytemuck::from_bytes(&self.data[start..start + size])
    }

    pub fn iter(&self) -> impl Iterator<Item = &SignedMsgOrderId> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }

    /// The route the signer chose for the order resting as `clob_order_id`.
    ///
    /// Absent means the order carries no signed route: it was placed directly,
    /// or its entry was reclaimed. Both read as unrouted, which is the same
    /// answer a zero digest has always given.
    pub fn route_for_clob_order(
        &self,
        clob_order_id: u64,
    ) -> Option<crate::state::order_params::RouteDigest> {
        if clob_order_id == 0 {
            return None;
        }
        self.iter()
            .find(|entry| entry.clob_order_id == clob_order_id)
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
        let size = std::mem::size_of::<SignedMsgOrderId>();
        let start = index as usize * size;
        bytemuck::from_bytes(&self.data[start..start + size])
    }

    pub fn get_mut(&mut self, index: u32) -> &mut SignedMsgOrderId {
        let size = std::mem::size_of::<SignedMsgOrderId>();
        let start = index as usize * size;
        bytemuck::from_bytes_mut(&mut self.data[start..start + size])
    }

    /// Replay check and stale sweep in one pass.
    ///
    /// An entry that names a live CLOB order is kept even once its `max_slot`
    /// is old: the remainder still rests, and the fill that resolves it reads
    /// the route from here. Such an entry is reclaimable only under the
    /// pressure `add_signed_msg_order_id` describes.
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

    /// Take the free slot, or the stalest retained one.
    ///
    /// A retained entry describes an order that may rest forever, because a
    /// signed limit order carries no expiry. Without a second pass those
    /// entries would fill the account and a user would stop being able to
    /// trade. So a full account reclaims the entry whose `max_slot` is oldest,
    /// which costs that order its route: the fill then treats it as unrouted
    /// and the taker's own limit price is what still bounds it. That is a lost
    /// guarantee under pressure, never a loss of funds, and it is bounded by
    /// how many orders the account holds.
    pub fn add_signed_msg_order_id(
        &mut self,
        signed_msg_order_id: SignedMsgOrderId,
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
            .filter(|i| self.get(*i).rests_on_clob())
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
    /// has been reclaimed. The caller places the order either way — a
    /// remainder without a route is an unrouted order, not a failed one.
    pub fn set_resting_route(
        &mut self,
        uuid: [u8; 8],
        clob_order_id: u64,
        route_digest: crate::state::order_params::RouteDigest,
    ) -> bool {
        for i in 0..self.len() {
            let entry = self.get_mut(i);
            if entry.uuid == uuid {
                entry.clob_order_id = clob_order_id;
                entry.route_digest = route_digest;
                return true;
            }
        }
        false
    }

    /// Release the entry's hold once its order has left the book, so the stale
    /// sweep can reclaim the slot in the ordinary way.
    pub fn clear_resting_route(&mut self, clob_order_id: u64) -> bool {
        if clob_order_id == 0 {
            return false;
        }
        for i in 0..self.len() {
            let entry = self.get_mut(i);
            if entry.clob_order_id == clob_order_id {
                entry.clob_order_id = 0;
                entry.route_digest = crate::state::order_params::NO_ROUTE_DIGEST;
                return true;
            }
        }
        false
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
