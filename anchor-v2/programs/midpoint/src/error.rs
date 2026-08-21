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

impl From<quoter_spec::SpecError> for MidpointError {
    /// A response this quoter's own region could not hold, or could not be
    /// read at, is a program bug rather than a caller's: the region size and
    /// every record stride are fixed at compile time, and
    /// `state::tests` pins the widest response against the region.
    fn from(error: quoter_spec::SpecError) -> Self {
        match error {
            quoter_spec::SpecError::DanglingCompletedOrder => MidpointError::InvariantViolated,
            _ => MidpointError::ResponseTooLarge,
        }
    }
}
