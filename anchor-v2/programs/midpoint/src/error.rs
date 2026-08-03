use anchor_lang_v2::prelude::*;

#[error_code]
pub enum MidpointError {
    #[msg("Signer does not match the quoter authority for this operation")]
    InvalidAuthority,
    #[msg("Quoter config out of bounds")]
    InvalidConfig,
    #[msg("More levels than the ladder holds")]
    TooManyLevels,
    #[msg("Level size is zero")]
    InvalidLevel,
    #[msg("Level offsets must be strictly ascending")]
    LevelsNotAscending,
    #[msg("Nonzero mid sequence must strictly increase")]
    StaleMidSequence,
    #[msg("Math overflow")]
    MathError,
    #[msg("Response exceeds the response buffer")]
    ResponseTooLarge,
    #[msg("Instructions sysvar account is not the sysvar")]
    InvalidInstructionsSysvar,
    // New variants go at the bottom: on-chain clients match error codes by
    // number.
    #[msg("Velocity state account is not velocity's initialized State")]
    InvalidVelocityState,
    #[msg("Post-operation invariant check failed")]
    InvariantViolated,
}
