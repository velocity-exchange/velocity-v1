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
    #[msg("An order older than the grace window has a user missing from the passed set")]
    StaleUserSet,
    #[msg("Side is below evict_threshold_per_side; nothing to evict")]
    BelowEvictThreshold,
    #[msg("Order is not expired")]
    OrderNotExpired,
}
