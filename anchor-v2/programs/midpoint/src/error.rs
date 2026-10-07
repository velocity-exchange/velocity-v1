use anchor_lang::prelude::*;

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
    /// @deprecated Nothing raises this. The numeric code stays so nothing else
    /// claims it.
    ///
    /// The protected-flow gate used to read a co-signature off the
    /// instructions sysvar. It now reads `taker_served_window` off the wire,
    /// which the caller asserts, so this program takes no sysvar account.
    #[msg("Deprecated: the protected-flow claim rides the wire, not a sysvar")]
    InvalidInstructionsSysvar,
    /// @deprecated Nothing raises this. The numeric code stays so nothing else
    /// claims it.
    ///
    /// The protected-flow gate used to read velocity's `State` account to find
    /// the flow authority. It now reads no velocity account.
    #[msg("Deprecated: this program reads no velocity account")]
    InvalidVelocityState,
    #[msg("Post-operation invariant check failed")]
    InvariantViolated,
    #[msg("User set holds more entries than USER_SET_CAPACITY")]
    OversizedUserSet,
    #[msg("An execute needs a reference price to check the band against")]
    MissingReferencePrice,
    // A new variant goes at the bottom. On-chain clients match error codes by
    // number.
}

impl From<quoter_spec::SpecError> for MidpointError {
    /// A response that this quoter's own region cannot hold, or cannot be read
    /// at, is a program bug and not a caller error. The region size and every
    /// record stride are fixed at compile time, and `state::tests` pins the
    /// widest response against the region.
    fn from(error: quoter_spec::SpecError) -> Self {
        match error {
            quoter_spec::SpecError::DanglingCompletedOrder
            | quoter_spec::SpecError::ChangeIndexOutOfRange => MidpointError::InvariantViolated,
            _ => MidpointError::ResponseTooLarge,
        }
    }
}
