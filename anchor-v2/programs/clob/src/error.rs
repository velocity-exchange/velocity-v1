use anchor_lang_v2::prelude::*;

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
    #[msg("Activation delay outside [default, max_activation_delay_slots]")]
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
    /// @deprecated Nothing raises this any more, and the numeric code stays
    /// so nothing else claims it.
    ///
    /// It used to mean two things, and both were refusals a caller could not
    /// act on. An order past the grace window whose owner was missing failed
    /// the call, which made a book with more makers than a transaction can
    /// carry unfillable by anyone; the walk ends there instead and reports
    /// what it was holding. A user set wider than `max_execute_users` also
    /// failed, which counted accounts that cost the response nothing, since
    /// `execute` bounds its own records where it writes them.
    #[msg("Deprecated: an unreachable maker ends the walk and is reported, it no longer fails")]
    StaleUserSet,
    #[msg("Side is below evict_threshold_per_side; nothing to evict")]
    BelowEvictThreshold,
    #[msg("Order is not expired")]
    OrderNotExpired,
    // Numeric codes are the on-chain identity of these errors: add new
    // variants at the bottom, never reorder or reuse.
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
    /// @deprecated Never returned. This was the first shape of the taker-origin
    /// gate, which failed any fill that reached a taker remainder a counterparty
    /// crossed. The gate now *skips* such an order the way it skips an expired
    /// one (see `book::TakerOriginGate`), so the depth behind it stays tradeable
    /// and a taker simply finds less depth than it hoped for — ordinary book
    /// behaviour, not an error. Kept in place because the numeric code is the
    /// on-chain identity of this and every later variant: do not delete or reuse.
    #[msg("Deprecated: a crossed taker-origin order is skipped, not rejected")]
    TakerOriginCrossPending,
    #[msg("User set holds more entries than USER_SET_CAPACITY")]
    OversizedUserSet,
    #[msg("Order would rest crossed with the opposite side and asked not to")]
    OrderWouldCross,
}

impl From<quoter_spec::SpecError> for ClobError {
    /// A response the market's own region could not hold or could not be read
    /// at is a program bug, not a caller's: the region size and every record
    /// stride are fixed at compile time, and `state`'s ceilings are what make
    /// both unreachable for a market whose config the init/update checks
    /// accepted. A dangling completed order is the same kind of bug one step
    /// further in — the walk named a balance change it never wrote.
    fn from(error: quoter_spec::SpecError) -> Self {
        match error {
            quoter_spec::SpecError::DanglingCompletedOrder => ClobError::BookInvariantViolated,
            _ => ClobError::ResponseTooLarge,
        }
    }
}
