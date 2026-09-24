use anchor_lang::prelude::*;

#[error_code]
pub enum ClobError {
    #[msg("Order price/size is zero or user is default")]
    InvalidOrderParams,
    #[msg("Order price is not a multiple of order_tick_size")]
    PriceNotTickAligned,
    #[msg("Order size is not a multiple of order_step_size")]
    SizeNotStepAligned,
    #[msg("Order size is below min_order_size")]
    OrderTooSmall,
    #[msg("Book side is at capacity and the order does not beat the tail")]
    SideAtCapacity,
    #[msg("Order ref is stale: node is free or holds a different order")]
    StaleOrderRef,
    #[msg("Order ref does not belong to the given user")]
    OrderUserMismatch,
    #[msg("Math overflow")]
    MathError,
    #[msg("Activation delay above max_activation_delay_slots")]
    InvalidActivationDelay,
    #[msg("max_ts is in the past")]
    MaxTsInPast,
    #[msg("Signer does not match the market authority for this operation")]
    InvalidAuthority,
    #[msg("Response exceeds the response buffer")]
    ResponseTooLarge,
    #[msg("Resize must grow the arena")]
    InvalidCapacity,
    #[msg("Market config out of bounds")]
    InvalidConfig,
    /// @deprecated Nothing raises this any more. The numeric code stays so
    /// nothing else claims it.
    ///
    /// It meant two things, and both were refusals a caller could not act on.
    /// An order past the grace window whose owner was missing failed the call.
    /// That made a book with more makers than one transaction can carry
    /// unfillable by anyone. The walk now ends at that order and reports what
    /// it holds. A user set wider than `max_execute_users` also failed. That
    /// counted accounts which cost the response nothing, because `execute`
    /// bounds its own records where it writes them.
    #[msg("Deprecated: an unreachable maker ends the walk and is reported, it no longer fails")]
    StaleUserSet,
    #[msg("Side is below evict_threshold_per_side; nothing to evict")]
    BelowEvictThreshold,
    #[msg("Order is not expired")]
    OrderNotExpired,
    // Numeric codes are the on-chain identity of these errors. Add a new
    // variant at the bottom. Never reorder or reuse one.
    #[msg("Node index is outside the order arena")]
    NodeIndexOutOfRange,
    #[msg("Order arena has no free node")]
    ArenaExhausted,
    #[msg("Book invariant check failed after the operation")]
    BookInvariantViolated,
    #[msg("Response level has a zero price or size, or is not strictly best-first")]
    InvalidResponseLevel,
    #[msg("Event payload exceeds its buffer")]
    EventTooLarge,
    /// @deprecated Never returned. This was the first shape of the crossing
    /// reservation. It failed any fill that reached a taker remainder which a
    /// counterparty crossed. The walk now skips claimed depth the way it skips
    /// an expired order. See `book::CrossReservation`. The depth behind the
    /// claimed order stays tradeable, and a taker finds less depth than it
    /// asked for, which is ordinary book behaviour. The variant stays because
    /// the numeric code is the on-chain identity of this and every later
    /// variant. Do not delete or reuse it.
    #[msg("Deprecated: a crossed taker-origin order is skipped, not rejected")]
    TakerOriginCrossPending,
    #[msg("User set holds more entries than USER_SET_CAPACITY")]
    OversizedUserSet,
    #[msg("Order would rest crossed with the opposite side and asked not to")]
    OrderWouldCross,
    /// A taker remainder rests for its activation window so counterparties can
    /// compete on price inside it, and its claim holds the depth it crosses for
    /// `reservation_grace_slots` after that. A taker that could withdraw while
    /// the claim holds would have a free option on that depth, at the cost of
    /// the makers who priced against it. The bind ends when the claim lapses.
    /// `max_ts` still expires the order, and liquidation passes `force`.
    #[msg("Taker-origin remainder is bound until its claim lapses")]
    TakerOriginBound,
    /// Only a taker remainder aggresses, so only a taker remainder can be
    /// filled from outside. An ordinary maker quote is filled by `execute`,
    /// where the book itself knows what it gave away.
    #[msg("Only a taker-origin order can be filled from outside the book")]
    OrderNotTakerOrigin,
    #[msg("Fill is larger than the order has left")]
    FillExceedsOrder,
    #[msg("max_ts falls inside the order's own activation delay")]
    MaxTsBeforeActivation,
    #[msg("Market still holds orders and cannot be closed")]
    MarketNotEmpty,
    #[msg("Order is expired or has not reached its activation slot")]
    OrderNotLive,
}

impl From<quoter_spec::SpecError> for ClobError {
    /// A response the region cannot hold, or cannot be read at, is a program
    /// bug rather than a caller error. Region size and record strides are fixed
    /// at compile time. `state`'s ceilings make both unreachable for a validly
    /// configured market. A dangling completed order is the same kind of bug.
    fn from(error: quoter_spec::SpecError) -> Self {
        match error {
            quoter_spec::SpecError::DanglingCompletedOrder => ClobError::BookInvariantViolated,
            _ => ClobError::ResponseTooLarge,
        }
    }
}
