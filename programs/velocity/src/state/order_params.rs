use {
    crate::{
        controller::position::PositionDirection,
        error::VelocityResult,
        math::{constants::DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION, safe_math::SafeMath},
        state::{
            events::OrderActionExplanation,
            perp_market::PerpMarket,
            user::{MarketType, OrderTriggerCondition, OrderType},
        },
    },
    anchor_lang::prelude::{
        borsh::{BorshDeserialize, BorshSerialize},
        *,
    },
};

#[cfg(test)]
mod tests;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Copy, Eq, PartialEq, Debug)]
pub struct OrderParams {
    pub order_type: OrderType,
    pub market_type: MarketType,
    pub direction: PositionDirection,
    pub user_order_id: u8,
    pub base_asset_amount: u64,
    pub price: u64,
    pub market_index: u16,
    pub reduce_only: bool,
    pub post_only: PostOnlyParam,
    pub bit_flags: u8,
    pub max_ts: Option<i64>,
    pub trigger_price: Option<u64>,
    pub trigger_condition: OrderTriggerCondition,
    pub oracle_price_offset: Option<i64>, // price offset from oracle for order
    /// How many slots the order's rested remainder waits before the book will
    /// take it. `None` takes the book's default. A longer wait gives more
    /// counterparties the chance to cross it, and a taker-origin remainder is
    /// crossed at the counterparty's price, so the wait can only improve the
    /// fill. The book caps it at `max_activation_delay_slots`, and a value
    /// below the default needs the flow-authority attestation. `max_ts` is a
    /// separate bound: it ends the order, and this delays when it can fill.
    /// A trigger order refuses it, because a slot stores no delay.
    pub activation_delay_slots: Option<u32>,
    /// the index into the placing user's RevenueShareEscrow.approved_builders list, if this order
    /// carries a builder code. Only honored for non-swift orders; swift orders carry the builder
    /// info in the signed message envelope instead.
    pub builder_idx: Option<u8>,
    /// the builder fee on this order, in tenths of a bps, e.g. 100 = 0.01%
    pub builder_fee_tenth_bps: Option<u16>,
}

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, PartialEq, Debug, Eq)]
#[borsh(use_discriminant = true)]
pub enum OrderParamsBitFlag {
    ImmediateOrCancel = 0b00000001,
}

impl OrderParams {
    pub fn get_close_perp_params(
        market: &PerpMarket,
        direction_to_close: PositionDirection,
        base_asset_amount: u64,
    ) -> VelocityResult<OrderParams> {
        // The close crosses the oracle by a market order's default slippage,
        // so it reaches liquidity rather than resting at the oracle.
        let twap = market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap;
        let slippage = twap.safe_div(DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION)?;
        let oracle_price_offset = match direction_to_close {
            PositionDirection::Long => slippage,
            PositionDirection::Short => -slippage,
        };

        Ok(OrderParams {
            market_type: MarketType::Perp,
            direction: direction_to_close,
            order_type: OrderType::Oracle,
            market_index: market.market_index,
            base_asset_amount,
            reduce_only: true,
            oracle_price_offset: Some(oracle_price_offset),
            ..OrderParams::default()
        })
    }

    pub fn is_immediate_or_cancel(&self) -> bool {
        self.bit_flags & OrderParamsBitFlag::ImmediateOrCancel as u8 != 0
    }

    pub fn is_max_leverage_order(&self) -> bool {
        self.base_asset_amount == u64::MAX
    }

    pub fn is_trigger_order(&self) -> bool {
        self.order_type == OrderType::TriggerMarket || self.order_type == OrderType::TriggerLimit
    }
}

/// Network tag on a signed message, checked against the build's own
/// cluster. Without it a devnet-signed message is byte-identical to a
/// mainnet one and could replay across clusters. The tag is required: an
/// absent tag replays exactly like a wrong one, so it is refused. Value is `b'm'` or `b'd'`.
pub const SIGNED_MSG_NETWORK_MAINNET: u8 = b'm';
pub const SIGNED_MSG_NETWORK_DEVNET: u8 = b'd';

/// The tag this build accepts.
pub const fn expected_signed_msg_network() -> u8 {
    #[cfg(feature = "mainnet-beta")]
    {
        SIGNED_MSG_NETWORK_MAINNET
    }
    #[cfg(not(feature = "mainnet-beta"))]
    {
        SIGNED_MSG_NETWORK_DEVNET
    }
}

/// Cap on a signed route. See [`SignedMsgOrderParamsMessage::route`]. A message naming more
/// custom quoters than this is refused, not truncated, and it bounds the message length for
/// off-chain buffers. It matches [`crate::state::prop_amm::MAX_ROUTE_QUOTERS`], since a taker
/// may name every entry one transaction can carry. Raising the cap later keeps every earlier message valid, since the field is a borsh `Vec`.
pub const MAX_SIGNED_MSG_ROUTE_LEN: usize = crate::state::prop_amm::MAX_ROUTE_QUOTERS;

/// Digest of a signed route: the `QuoterV0` entries a taker chose, reduced
/// to the bytes [`crate::state::signed_msg_user::SignedMsgOrderId`] can
/// hold. An order stores it to pin which entries a fill may claim its
/// signer chose. The route is canonicalised first, by sorting and removing duplicates, so the same choice always digests the same way, and an empty route digests to zero so one equality check covers both an unsigned and a signed route.
pub type RouteDigest = [u8; ROUTE_DIGEST_LEN];

/// Width of a [`RouteDigest`]. See [`route_digest`] for why it is this wide.
pub const ROUTE_DIGEST_LEN: usize = 8;

/// The digest of no route. A directly-placed order holds this, so one equality
/// check covers both an unsigned route and the route that was signed.
pub const NO_ROUTE_DIGEST: RouteDigest = [0; ROUTE_DIGEST_LEN];

/// Reduce `route` to a [`RouteDigest`]. Eight bytes, because a filler can
/// only claim entries the transaction carries, at most `MAX_ROUTE_QUOTERS`
/// of them, so its candidate set is every subset of that size over the
/// market's registered entries. That set is public and worth precomputing,
/// and the width targets a larger future entry count. A collision routes
/// to an entry the taker did not pick. The taker's own limit price bounds
/// that, so it is not a theft, but it still defeats the digest's purpose.
/// The width matches [`crate::state::signed_msg_user::SignedMsgOrderId`]'s stride. `Order` allows only five bytes, since a sixth byte would shift that array's stride in `User`.
pub fn route_digest(route: &[Pubkey]) -> RouteDigest {
    if route.is_empty() {
        return [0; ROUTE_DIGEST_LEN];
    }

    let mut keys: Vec<[u8; 32]> = route.iter().map(|key| key.to_bytes()).collect();
    keys.sort_unstable();
    keys.dedup();
    let flat: Vec<u8> = keys.concat();
    let hash = solana_program::hash::hash(&flat);
    let mut out = [0u8; ROUTE_DIGEST_LEN];
    out.copy_from_slice(&hash.to_bytes()[..ROUTE_DIGEST_LEN]);
    // A real route must stay distinguishable from an absent one, so it never
    // takes the no-route value.
    if out == [0; ROUTE_DIGEST_LEN] {
        out[0] = 1;
    }

    out
}

/// Trailing fields are appended, never inserted. The verifier zero-pads a
/// short payload before decoding, so an older producer's message reads as
/// `None` for everything it did not send. See
/// `validation::sig_verification`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Eq, PartialEq, Debug)]
pub struct SignedMsgOrderParamsMessage {
    pub signed_msg_order_params: OrderParams,
    pub sub_account_id: u16,
    pub slot: u64,
    pub uuid: [u8; 8],
    pub take_profit_order_params: Option<SignedMsgTriggerOrderParams>,
    pub stop_loss_order_params: Option<SignedMsgTriggerOrderParams>,
    pub max_margin_ratio: Option<u16>,
    pub builder_idx: Option<u8>,
    pub builder_fee_tenth_bps: Option<u16>,
    pub isolated_position_deposit: Option<u64>,
    /// [`SIGNED_MSG_NETWORK_MAINNET`] / [`SIGNED_MSG_NETWORK_DEVNET`].
    pub network: Option<u8>,
    /// The `QuoterV0` entries of the custom quoters the taker signed for. The
    /// CLOB and vAMM are the baseline of every router fill, so no route names
    /// them. The placing keeper must carry every quoter the route names, and
    /// the order's record keeps the route digest for a later fill.
    pub route: Option<Vec<Pubkey>>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Eq, PartialEq, Debug)]
pub struct SignedMsgOrderParamsDelegateMessage {
    pub signed_msg_order_params: OrderParams,
    pub taker_pubkey: Pubkey,
    pub slot: u64,
    pub uuid: [u8; 8],
    pub take_profit_order_params: Option<SignedMsgTriggerOrderParams>,
    pub stop_loss_order_params: Option<SignedMsgTriggerOrderParams>,
    pub max_margin_ratio: Option<u16>,
    pub builder_idx: Option<u8>,
    pub builder_fee_tenth_bps: Option<u16>,
    pub isolated_position_deposit: Option<u64>,
    /// See [`SignedMsgOrderParamsMessage::network`].
    pub network: Option<u8>,
    /// See [`SignedMsgOrderParamsMessage::route`].
    pub route: Option<Vec<Pubkey>>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Eq, PartialEq, Debug)]
pub struct SignedMsgTriggerOrderParams {
    pub trigger_price: u64,
    pub base_asset_amount: u64,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq, Default)]
pub enum PostOnlyParam {
    #[default]
    None,
    MustPostOnly, // Tx fails if order can't be post only
    TryPostOnly,  // Tx succeeds and order not placed if can't be post only
    Slide,        // Modify price to be post only if can't be post only
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct ModifyOrderParams {
    pub direction: Option<PositionDirection>,
    pub base_asset_amount: Option<u64>,
    pub price: Option<u64>,
    pub reduce_only: Option<bool>,
    pub post_only: Option<PostOnlyParam>,
    /// Unused. The modify reads no bit flags. The field stays because it is
    /// part of the instruction's arguments.
    pub bit_flags: Option<u8>,
    pub max_ts: Option<i64>,
    pub trigger_price: Option<u64>,
    pub trigger_condition: Option<OrderTriggerCondition>,
    pub oracle_price_offset: Option<i64>,
    pub policy: Option<u8>,
}

impl ModifyOrderParams {
    pub fn must_modify(&self) -> bool {
        self.policy.unwrap_or(0) & ModifyOrderPolicy::MustModify as u8 != 0
    }

    pub fn exclude_previous_fill(&self) -> bool {
        self.policy.unwrap_or(0) & ModifyOrderPolicy::ExcludePreviousFill as u8 != 0
    }
}

pub enum ModifyOrderPolicy {
    MustModify = 1,
    ExcludePreviousFill = 2,
}

#[derive(Clone)]
pub struct PlaceOrderOptions {
    pub signed_msg_taker_order_slot: Option<u64>,
    pub try_expire_orders: bool,
    pub enforce_margin_check: bool,
    pub risk_increasing: bool,
    pub explanation: OrderActionExplanation,
    pub existing_position_direction_override: Option<PositionDirection>,
    /// Emit the `Place` `OrderActionRecord` and `OrderRecord` for the built
    /// order. A maker resting straight on the CLOB clears this flag: its
    /// CLOB placement record is the one statement about the order, and an
    /// detached place record would duplicate it.
    pub emit_place_record: bool,
}

impl Default for PlaceOrderOptions {
    fn default() -> Self {
        Self {
            signed_msg_taker_order_slot: None,
            try_expire_orders: true,
            enforce_margin_check: true,
            risk_increasing: false,
            explanation: OrderActionExplanation::None,
            existing_position_direction_override: None,
            emit_place_record: true,
        }
    }
}

impl PlaceOrderOptions {
    pub fn update_risk_increasing(&mut self, risk_increasing: bool) {
        self.risk_increasing = self.risk_increasing || risk_increasing;
    }

    pub fn explanation(mut self, explanation: OrderActionExplanation) -> Self {
        self.explanation = explanation;
        self
    }

    pub fn is_liquidation(&self) -> bool {
        self.explanation == OrderActionExplanation::Liquidation
    }

    pub fn set_order_slot(&mut self, slot: u64) {
        self.signed_msg_taker_order_slot = Some(slot);
    }

    pub fn get_order_slot(&self, order_slot: u64) -> u64 {
        let mut min_order_slot = order_slot;
        if let Some(signed_msg_taker_order_slot) = self.signed_msg_taker_order_slot {
            min_order_slot = order_slot.min(signed_msg_taker_order_slot);
        }
        min_order_slot
    }

    pub fn is_signed_msg_order(&self) -> bool {
        self.signed_msg_taker_order_slot.is_some()
    }
}

/// What a place-and-take must fill. The instruction reverts when the take
/// falls short of it.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Eq, PartialEq, Debug)]
pub enum PlaceAndTakeOrderSuccessCondition {
    PartialFill,
    FullFill,
}
