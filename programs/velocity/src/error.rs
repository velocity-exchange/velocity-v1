use anchor_lang::prelude::*;
pub type VelocityResult<T = ()> = std::result::Result<T, ErrorCode>;

/// Anchor encodes custom errors as: `error_code = 6000 + discriminant`
/// where `discriminant` is the 0-based index of the variant in this enum.
///
/// To decode a hex error from logs (e.g. `custom program error: 0x18b6`):
///   1. Convert hex → decimal:  0x18b6 = 6326
///   2. Subtract 6000:          6326 - 6000 = 326
///   3. Count down to index 326 in this enum (use the `// --- index N` markers below)
///
/// To encode a known variant → hex:
///   error_code = 6000 + index,  then format as hex
///
/// Index markers are placed every 50 variants to speed up counting.
/// Example: `SlippageOutsideLimit` is at index 15  → 6000+15 = 6015 = 0x177F
#[error_code]
#[derive(PartialEq, Eq)]
pub enum ErrorCode {
    // --- index 0 (error 0x1770 / 6000) ---
    #[msg("Invalid Spot Market Authority")]
    InvalidSpotMarketAuthority,
    #[msg("Clearing house not insurance fund authority")]
    InvalidInsuranceFundAuthority,
    #[msg("Insufficient deposit")]
    InsufficientDeposit,
    #[msg("Insufficient collateral")]
    InsufficientCollateral,
    #[msg("Sufficient collateral")]
    SufficientCollateral,
    #[msg("Max number of positions taken")]
    MaxNumberOfPositions,
    #[msg("Admin Controls Prices Disabled")]
    AdminControlsPricesDisabled,
    #[msg("Market Delisted")]
    MarketDelisted,
    #[msg("Market Index Already Initialized")]
    MarketIndexAlreadyInitialized,
    #[msg("User Account And User Positions Account Mismatch")]
    UserAccountAndUserPositionsAccountMismatch,
    #[msg("User Has No Position In Market")]
    UserHasNoPositionInMarket,
    #[msg("Invalid Initial Peg")]
    InvalidInitialPeg,
    #[msg("AMM repeg already configured with amt given")]
    InvalidRepegRedundant,
    #[msg("AMM repeg incorrect repeg direction")]
    InvalidRepegDirection,
    #[msg("AMM repeg out of bounds pnl")]
    InvalidRepegProfitability,
    #[msg("Slippage Outside Limit Price")]
    SlippageOutsideLimit,
    #[msg("Order Size Too Small")]
    OrderSizeTooSmall,
    #[msg("Price change too large when updating K")]
    InvalidUpdateK,
    #[msg("Admin tried to withdraw amount larger than fees collected")]
    AdminWithdrawTooLarge,
    #[msg("Math Error")]
    MathError,
    #[msg("Conversion to u128/u64 failed with an overflow or underflow")]
    BnConversionError,
    #[msg("Clock unavailable")]
    ClockUnavailable,
    #[msg("Unable To Load Oracles")]
    UnableToLoadOracle,
    #[msg("Price Bands Breached")]
    PriceBandsBreached,
    #[msg("Exchange is paused")]
    ExchangePaused,
    #[msg("Invalid whitelist token")]
    InvalidWhitelistToken,
    #[msg("Whitelist token not found")]
    WhitelistTokenNotFound,
    #[msg("Invalid discount token")]
    InvalidDiscountToken,
    #[msg("Discount token not found")]
    DiscountTokenNotFound,
    #[msg("Referrer not found")]
    ReferrerNotFound,
    #[msg("ReferrerNotFound")]
    ReferrerStatsNotFound,
    #[msg("ReferrerMustBeWritable")]
    ReferrerMustBeWritable,
    #[msg("ReferrerMustBeWritable")]
    ReferrerStatsMustBeWritable,
    #[msg("ReferrerAndReferrerStatsAuthorityUnequal")]
    ReferrerAndReferrerStatsAuthorityUnequal,
    #[msg("InvalidReferrer")]
    InvalidReferrer,
    #[msg("InvalidOracle")]
    InvalidOracle,
    #[msg("OracleNotFound")]
    OracleNotFound,
    #[msg("Liquidations Blocked By Oracle")]
    LiquidationsBlockedByOracle,
    #[msg("Can not deposit more than max deposit")]
    MaxDeposit,
    #[msg("Can not delete user that still has collateral")]
    CantDeleteUserWithCollateral,
    #[msg("AMM funding out of bounds pnl")]
    InvalidFundingProfitability,
    #[msg("Casting Failure")]
    CastingFailure,
    #[msg("InvalidOrder")]
    InvalidOrder,
    #[msg("InvalidOrderMaxTs")]
    InvalidOrderMaxTs,
    #[msg("InvalidOrderMarketType")]
    InvalidOrderMarketType,
    #[msg("InvalidOrderForInitialMarginReq")]
    InvalidOrderForInitialMarginReq,
    #[msg("InvalidOrderNotRiskReducing")]
    InvalidOrderNotRiskReducing,
    #[msg("InvalidOrderSizeTooSmall")]
    InvalidOrderSizeTooSmall,
    #[msg("InvalidOrderNotStepSizeMultiple")]
    InvalidOrderNotStepSizeMultiple,
    #[msg("InvalidOrderBaseQuoteAsset")]
    InvalidOrderBaseQuoteAsset,
    // --- index 50 (error 0x17D2 / 6050) ---
    #[msg("InvalidOrderIOC")]
    InvalidOrderIOC,
    #[msg("InvalidOrderPostOnly")]
    InvalidOrderPostOnly,
    #[msg("InvalidOrderIOCPostOnly")]
    InvalidOrderIOCPostOnly,
    #[msg("InvalidOrderTrigger")]
    InvalidOrderTrigger,
    #[msg("InvalidOrderAuction")]
    InvalidOrderAuction,
    #[msg("InvalidOrderOracleOffset")]
    InvalidOrderOracleOffset,
    #[msg("InvalidOrderMinOrderSize")]
    InvalidOrderMinOrderSize,
    #[msg("Failed to Place Post-Only Limit Order")]
    PlacePostOnlyLimitFailure,
    #[msg("User has no order")]
    UserHasNoOrder,
    #[msg("Order Amount Too Small")]
    OrderAmountTooSmall,
    #[msg("Max number of orders taken")]
    MaxNumberOfOrders,
    #[msg("Order does not exist")]
    OrderDoesNotExist,
    #[msg("Order not open")]
    OrderNotOpen,
    #[msg("FillOrderDidNotUpdateState")]
    FillOrderDidNotUpdateState,
    #[msg("Reduce only order increased risk")]
    ReduceOnlyOrderIncreasedRisk,
    #[msg("Unable to load AccountLoader")]
    UnableToLoadAccountLoader,
    #[msg("Trade Size Too Large")]
    TradeSizeTooLarge,
    #[msg("User cant refer themselves")]
    UserCantReferThemselves,
    #[msg("Did not receive expected referrer")]
    DidNotReceiveExpectedReferrer,
    #[msg("Could not deserialize referrer")]
    CouldNotDeserializeReferrer,
    #[msg("Could not deserialize referrer stats")]
    CouldNotDeserializeReferrerStats,
    #[msg("User Order Id Already In Use")]
    UserOrderIdAlreadyInUse,
    #[msg("No positions liquidatable")]
    NoPositionsLiquidatable,
    #[msg("Invalid Margin Ratio")]
    InvalidMarginRatio,
    #[msg("Cant Cancel Post Only Order")]
    CantCancelPostOnlyOrder,
    #[msg("InvalidOracleOffset")]
    InvalidOracleOffset,
    #[msg("CantExpireOrders")]
    CantExpireOrders,
    #[msg("CouldNotLoadMarketData")]
    CouldNotLoadMarketData,
    #[msg("PerpMarketNotFound")]
    PerpMarketNotFound,
    #[msg("InvalidMarketAccount")]
    InvalidMarketAccount,
    #[msg("UnableToLoadMarketAccount")]
    UnableToLoadPerpMarketAccount,
    #[msg("MarketWrongMutability")]
    MarketWrongMutability,
    #[msg("UnableToCastUnixTime")]
    UnableToCastUnixTime,
    #[msg("CouldNotFindSpotPosition")]
    CouldNotFindSpotPosition,
    #[msg("NoSpotPositionAvailable")]
    NoSpotPositionAvailable,
    #[msg("InvalidSpotMarketInitialization")]
    InvalidSpotMarketInitialization,
    #[msg("CouldNotLoadSpotMarketData")]
    CouldNotLoadSpotMarketData,
    #[msg("SpotMarketNotFound")]
    SpotMarketNotFound,
    #[msg("InvalidSpotMarketAccount")]
    InvalidSpotMarketAccount,
    #[msg("UnableToLoadSpotMarketAccount")]
    UnableToLoadSpotMarketAccount,
    #[msg("SpotMarketWrongMutability")]
    SpotMarketWrongMutability,
    #[msg("SpotInterestNotUpToDate")]
    SpotMarketInterestNotUpToDate,
    #[msg("SpotMarketInsufficientDeposits")]
    SpotMarketInsufficientDeposits,
    #[msg("UserMustSettleTheirOwnPositiveUnsettledPNL")]
    UserMustSettleTheirOwnPositiveUnsettledPNL,
    #[msg("CantUpdateSpotBalanceType")]
    CantUpdateSpotBalanceType,
    #[msg("InsufficientCollateralForSettlingPNL")]
    InsufficientCollateralForSettlingPNL,
    #[msg("AMMNotUpdatedInSameSlot")]
    AMMNotUpdatedInSameSlot,
    #[msg("AuctionNotComplete")]
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    AuctionNotComplete,
    #[msg("MakerNotFound")]
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    MakerNotFound,
    #[msg("MakerNotFound")]
    MakerStatsNotFound,
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    // --- index 100 (error 0x1834 / 6100) ---
    #[msg("MakerMustBeWritable")]
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    MakerMustBeWritable,
    #[msg("MakerMustBeWritable")]
    MakerStatsMustBeWritable,
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    #[msg("MakerOrderNotFound")]
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    MakerOrderNotFound,
    #[msg("CouldNotDeserializeMaker")]
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    CouldNotDeserializeMaker,
    #[msg("CouldNotDeserializeMaker")]
    CouldNotDeserializeMakerStats,
    #[msg("AuctionPriceDoesNotSatisfyMaker")]
    AuctionPriceDoesNotSatisfyMaker,
    #[msg("MakerCantFulfillOwnOrder")]
    MakerCantFulfillOwnOrder,
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    #[msg("MakerOrderMustBePostOnly")]
    MakerOrderMustBePostOnly,
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    #[msg("CantMatchTwoPostOnlys")]
    CantMatchTwoPostOnlys,
    #[msg("OrderBreachesOraclePriceLimits")]
    OrderBreachesOraclePriceLimits,
    #[msg("OrderMustBeTriggeredFirst")]
    OrderMustBeTriggeredFirst,
    #[msg("OrderNotTriggerable")]
    OrderNotTriggerable,
    #[msg("OrderDidNotSatisfyTriggerCondition")]
    OrderDidNotSatisfyTriggerCondition,
    #[msg("PositionAlreadyBeingLiquidated")]
    PositionAlreadyBeingLiquidated,
    #[msg("PositionDoesntHaveOpenPositionOrOrders")]
    PositionDoesntHaveOpenPositionOrOrders,
    #[msg("AllOrdersAreAlreadyLiquidations")]
    AllOrdersAreAlreadyLiquidations,
    #[msg("CantCancelLiquidationOrder")]
    CantCancelLiquidationOrder,
    #[msg("UserIsBeingLiquidated")]
    UserIsBeingLiquidated,
    #[msg("LiquidationsOngoing")]
    LiquidationsOngoing,
    #[msg("WrongSpotBalanceType")]
    WrongSpotBalanceType,
    #[msg("UserCantLiquidateThemself")]
    UserCantLiquidateThemself,
    #[msg("InvalidPerpPositionToLiquidate")]
    InvalidPerpPositionToLiquidate,
    #[msg("InvalidBaseAssetAmountForLiquidatePerp")]
    InvalidBaseAssetAmountForLiquidatePerp,
    #[msg("InvalidPositionLastFundingRate")]
    InvalidPositionLastFundingRate,
    #[msg("InvalidPositionDelta")]
    InvalidPositionDelta,
    #[msg("UserBankrupt")]
    UserBankrupt,
    #[msg("UserNotBankrupt")]
    UserNotBankrupt,
    #[msg("UserHasInvalidBorrow")]
    UserHasInvalidBorrow,
    #[msg("DailyWithdrawLimit")]
    DailyWithdrawLimit,
    #[msg("DefaultError")]
    DefaultError,
    /// @deprecated vAMM LP removed.
    #[msg("Insufficient LP tokens")]
    InsufficientLPTokens,
    /// @deprecated vAMM LP removed.
    #[msg("Cant LP with a market position")]
    CantLPWithPerpPosition,
    /// @deprecated vAMM LP removed.
    #[msg("Unable to burn LP tokens")]
    UnableToBurnLPTokens,
    #[msg("Trying to remove liqudity too fast after adding it")]
    TryingToRemoveLiquidityTooFast,
    #[msg("Invalid Spot Market Vault")]
    InvalidSpotMarketVault,
    #[msg("Invalid Spot Market State")]
    InvalidSpotMarketState,
    /// @deprecated removed with spot fulfillment infra
    #[msg("InvalidSerumProgram")]
    InvalidSerumProgram,
    /// @deprecated removed with spot fulfillment infra
    #[msg("InvalidSerumMarket")]
    InvalidSerumMarket,
    /// @deprecated removed with spot fulfillment infra
    #[msg("InvalidSerumBids")]
    InvalidSerumBids,
    /// @deprecated removed with spot fulfillment infra
    #[msg("InvalidSerumAsks")]
    InvalidSerumAsks,
    /// @deprecated removed with spot fulfillment infra
    #[msg("InvalidSerumOpenOrders")]
    InvalidSerumOpenOrders,
    /// @deprecated removed with spot fulfillment infra
    #[msg("FailedSerumCPI")]
    FailedSerumCPI,
    #[msg("FailedToFillOnExternalMarket")]
    FailedToFillOnExternalMarket,
    #[msg("InvalidFulfillmentConfig")]
    InvalidFulfillmentConfig,
    #[msg("InvalidFeeStructure")]
    InvalidFeeStructure,
    #[msg("Insufficient IF shares")]
    InsufficientIFShares,
    #[msg("the Market has paused this action")]
    MarketActionPaused,
    #[msg("the Market status doesnt allow placing orders")]
    MarketPlaceOrderPaused,
    #[msg("the Market status doesnt allow filling orders")]
    MarketFillOrderPaused,
    #[msg("the Market status doesnt allow withdraws")]
    MarketWithdrawPaused,
    // --- index 150 (error 0x1866 / 6150) ---
    #[msg("Action violates the Protected Asset Tier rules")]
    ProtectedAssetTierViolation,
    #[msg("Action violates the Isolated Asset Tier rules")]
    IsolatedAssetTierViolation,
    #[msg("User Cant Be Deleted")]
    UserCantBeDeleted,
    #[msg("Reduce Only Withdraw Increased Risk")]
    ReduceOnlyWithdrawIncreasedRisk,
    #[msg("Max Open Interest")]
    MaxOpenInterest,
    #[msg("Cant Resolve Perp Bankruptcy")]
    CantResolvePerpBankruptcy,
    #[msg("Liquidation Doesnt Satisfy Limit Price")]
    LiquidationDoesntSatisfyLimitPrice,
    #[msg("Margin Trading Disabled")]
    MarginTradingDisabled,
    #[msg("Invalid Market Status to Settle Perp Pnl")]
    InvalidMarketStatusToSettlePnl,
    #[msg("PerpMarketNotInSettlement")]
    PerpMarketNotInSettlement,
    #[msg("PerpMarketNotInReduceOnly")]
    PerpMarketNotInReduceOnly,
    #[msg("PerpMarketSettlementBufferNotReached")]
    PerpMarketSettlementBufferNotReached,
    #[msg("PerpMarketSettlementUserHasOpenOrders")]
    PerpMarketSettlementUserHasOpenOrders,
    /// @deprecated vAMM LP removed.
    #[msg("PerpMarketSettlementUserHasActiveLP")]
    PerpMarketSettlementUserHasActiveLP,
    #[msg("UnableToSettleExpiredUserPosition")]
    UnableToSettleExpiredUserPosition,
    #[msg("UnequalMarketIndexForSpotTransfer")]
    UnequalMarketIndexForSpotTransfer,
    #[msg("InvalidPerpPositionDetected")]
    InvalidPerpPositionDetected,
    #[msg("InvalidSpotPositionDetected")]
    InvalidSpotPositionDetected,
    #[msg("InvalidAmmDetected")]
    InvalidAmmDetected,
    #[msg("InvalidAmmForFillDetected")]
    InvalidAmmForFillDetected,
    #[msg("InvalidAmmLimitPriceOverride")]
    InvalidAmmLimitPriceOverride,
    #[msg("InvalidOrderFillPrice")]
    InvalidOrderFillPrice,
    #[msg("SpotMarketBalanceInvariantViolated")]
    SpotMarketBalanceInvariantViolated,
    #[msg("SpotMarketVaultInvariantViolated")]
    SpotMarketVaultInvariantViolated,
    #[msg("InvalidPDA")]
    InvalidPDA,
    #[msg("InvalidPDASigner")]
    InvalidPDASigner,
    #[msg("RevenueSettingsCannotSettleToIF")]
    RevenueSettingsCannotSettleToIF,
    #[msg("NoRevenueToSettleToIF")]
    NoRevenueToSettleToIF,
    #[msg("NoAmmPerpPnlDeficit")]
    NoAmmPerpPnlDeficit,
    #[msg("SufficientPerpPnlPool")]
    SufficientPerpPnlPool,
    #[msg("InsufficientPerpPnlPool")]
    InsufficientPerpPnlPool,
    #[msg("PerpPnlDeficitBelowThreshold")]
    PerpPnlDeficitBelowThreshold,
    #[msg("MaxRevenueWithdrawPerPeriodReached")]
    MaxRevenueWithdrawPerPeriodReached,
    #[msg("InvalidSpotPositionDetected")]
    MaxIFWithdrawReached,
    #[msg("NoIFWithdrawAvailable")]
    NoIFWithdrawAvailable,
    #[msg("InvalidIFUnstake")]
    InvalidIFUnstake,
    #[msg("InvalidIFUnstakeSize")]
    InvalidIFUnstakeSize,
    #[msg("InvalidIFUnstakeCancel")]
    InvalidIFUnstakeCancel,
    #[msg("InvalidIFForNewStakes")]
    InvalidIFForNewStakes,
    #[msg("InvalidIFRebase")]
    InvalidIFRebase,
    #[msg("InvalidInsuranceUnstakeSize")]
    InvalidInsuranceUnstakeSize,
    #[msg("InvalidOrderLimitPrice")]
    InvalidOrderLimitPrice,
    #[msg("InvalidIFDetected")]
    InvalidIFDetected,
    #[msg("InvalidAmmMaxSpreadDetected")]
    InvalidAmmMaxSpreadDetected,
    #[msg("InvalidConcentrationCoef")]
    InvalidConcentrationCoef,
    #[msg("InvalidSrmVault")]
    InvalidSrmVault,
    #[msg("InvalidVaultOwner")]
    InvalidVaultOwner,
    #[msg("InvalidMarketStatusForFills")]
    InvalidMarketStatusForFills,
    #[msg("IFWithdrawRequestInProgress")]
    IFWithdrawRequestInProgress,
    #[msg("NoIFWithdrawRequestInProgress")]
    NoIFWithdrawRequestInProgress,
    // --- index 200 (error 0x1898 / 6200) ---
    #[msg("IFWithdrawRequestTooSmall")]
    IFWithdrawRequestTooSmall,
    #[msg("IncorrectSpotMarketAccountPassed")]
    IncorrectSpotMarketAccountPassed,
    #[msg("BlockchainClockInconsistency")]
    BlockchainClockInconsistency,
    #[msg("InvalidIFSharesDetected")]
    InvalidIFSharesDetected,
    /// @deprecated vAMM LP removed.
    #[msg("NewLPSizeTooSmall")]
    NewLPSizeTooSmall,
    /// @deprecated vAMM LP removed.
    #[msg("MarketStatusInvalidForNewLP")]
    MarketStatusInvalidForNewLP,
    #[msg("InvalidMarkTwapUpdateDetected")]
    InvalidMarkTwapUpdateDetected,
    #[msg("MarketSettlementAttemptOnActiveMarket")]
    MarketSettlementAttemptOnActiveMarket,
    /// @deprecated vAMM LP removed.
    #[msg("MarketSettlementRequiresSettledLP")]
    MarketSettlementRequiresSettledLP,
    #[msg("MarketSettlementAttemptTooEarly")]
    MarketSettlementAttemptTooEarly,
    #[msg("MarketSettlementTargetPriceInvalid")]
    MarketSettlementTargetPriceInvalid,
    #[msg("UnsupportedSpotMarket")]
    UnsupportedSpotMarket,
    #[msg("SpotOrdersDisabled")]
    SpotOrdersDisabled,
    #[msg("Market Being Initialized")]
    MarketBeingInitialized,
    #[msg("Invalid Sub Account Id")]
    InvalidUserSubAccountId,
    #[msg("Invalid Trigger Order Condition")]
    InvalidTriggerOrderCondition,
    #[msg("Invalid Spot Position")]
    InvalidSpotPosition,
    #[msg("Cant transfer between same user account")]
    CantTransferBetweenSameUserAccount,
    #[msg("Invalid Perp Position")]
    InvalidPerpPosition,
    #[msg("Unable To Get Limit Price")]
    UnableToGetLimitPrice,
    #[msg("Invalid Liquidation")]
    InvalidLiquidation,
    #[msg("Spot Fulfillment Config Disabled")]
    SpotFulfillmentConfigDisabled,
    #[msg("Invalid Maker")]
    InvalidMaker,
    #[msg("Failed Unwrap")]
    FailedUnwrap,
    #[msg("Max Number Of Users")]
    MaxNumberOfUsers,
    #[msg("InvalidOracleForSettlePnl")]
    InvalidOracleForSettlePnl,
    #[msg("MarginOrdersOpen")]
    MarginOrdersOpen,
    #[msg("TierViolationLiquidatingPerpPnl")]
    TierViolationLiquidatingPerpPnl,
    #[msg("CouldNotLoadUserData")]
    CouldNotLoadUserData,
    #[msg("UserWrongMutability")]
    UserWrongMutability,
    #[msg("InvalidUserAccount")]
    InvalidUserAccount,
    #[msg("CouldNotLoadUserData")]
    CouldNotLoadUserStatsData,
    #[msg("UserWrongMutability")]
    UserStatsWrongMutability,
    #[msg("InvalidUserAccount")]
    InvalidUserStatsAccount,
    #[msg("UserNotFound")]
    UserNotFound,
    #[msg("UnableToLoadUserAccount")]
    UnableToLoadUserAccount,
    #[msg("UserStatsNotFound")]
    UserStatsNotFound,
    #[msg("UnableToLoadUserStatsAccount")]
    UnableToLoadUserStatsAccount,
    #[msg("User Not Inactive")]
    UserNotInactive,
    /// @deprecated No path produces this. The DLOB it belonged to is gone.
    #[msg("RevertFill")]
    RevertFill,
    #[msg("Invalid MarketAccount for Deletion")]
    InvalidMarketAccountforDeletion,
    #[msg("Invalid Spot Fulfillment Params")]
    InvalidSpotFulfillmentParams,
    #[msg("Failed to Get Mint")]
    FailedToGetMint,
    /// @deprecated removed with spot fulfillment infra
    #[msg("FailedPhoenixCPI")]
    FailedPhoenixCPI,
    /// @deprecated removed with spot fulfillment infra
    #[msg("FailedToDeserializePhoenixMarket")]
    FailedToDeserializePhoenixMarket,
    #[msg("InvalidPricePrecision")]
    InvalidPricePrecision,
    /// @deprecated removed with spot fulfillment infra
    #[msg("InvalidPhoenixProgram")]
    InvalidPhoenixProgram,
    /// @deprecated removed with spot fulfillment infra
    #[msg("InvalidPhoenixMarket")]
    InvalidPhoenixMarket,
    #[msg("InvalidSwap")]
    InvalidSwap,
    #[msg("SwapLimitPriceBreached")]
    SwapLimitPriceBreached,
    // --- index 250 (error 0x18CA / 6250) ---
    #[msg("SpotMarketReduceOnly")]
    SpotMarketReduceOnly,
    #[msg("FundingWasNotUpdated")]
    FundingWasNotUpdated,
    #[msg("ImpossibleFill")]
    ImpossibleFill,
    #[msg("CantUpdatePerpBidAskTwap")]
    CantUpdatePerpBidAskTwap,
    #[msg("UserReduceOnly")]
    UserReduceOnly,
    #[msg("InvalidMarginCalculation")]
    InvalidMarginCalculation,
    #[msg("CantPayUserInitFee")]
    CantPayUserInitFee,
    #[msg("CantReclaimRent")]
    CantReclaimRent,
    #[msg("InsuranceFundOperationPaused")]
    InsuranceFundOperationPaused,
    #[msg("NoUnsettledPnl")]
    NoUnsettledPnl,
    #[msg("PnlPoolCantSettleUser")]
    PnlPoolCantSettleUser,
    #[msg("OracleInvalid")]
    OracleNonPositive,
    #[msg("OracleTooVolatile")]
    OracleTooVolatile,
    #[msg("OracleTooUncertain")]
    OracleTooUncertain,
    #[msg("OracleStaleForMargin")]
    OracleStaleForMargin,
    #[msg("OracleInsufficientDataPoints")]
    OracleInsufficientDataPoints,
    #[msg("OracleStaleForAMM")]
    OracleStaleForAMM,
    #[msg("Unable to parse pull oracle message")]
    UnableToParsePullOracleMessage,
    #[msg("Can not borow more than max borrows")]
    MaxBorrows,
    #[msg("Updates must be monotonically increasing")]
    OracleUpdatesNotMonotonic,
    #[msg("Trying to update price feed with the wrong feed id")]
    OraclePriceFeedMessageMismatch,
    #[msg("The message in the update must be a PriceFeedMessage")]
    OracleUnsupportedMessageType,
    #[msg("Could not deserialize the message in the update")]
    OracleDeserializeMessageFailed,
    #[msg("Wrong guardian set owner in update price atomic")]
    OracleWrongGuardianSetOwner,
    #[msg("Oracle post update atomic price feed account must be velocity program")]
    OracleWrongWriteAuthority,
    #[msg("Oracle vaa owner must be wormhole program")]
    OracleWrongVaaOwner,
    #[msg("Multi updates must have 2 or fewer accounts passed in remaining accounts")]
    OracleTooManyPriceAccountUpdates,
    #[msg("Don't have the same remaining accounts number and pyth updates left")]
    OracleMismatchedVaaAndPriceUpdates,
    #[msg("Remaining account passed does not match oracle update derived pda")]
    OracleBadRemainingAccountPublicKey,
    /// @deprecated removed with spot fulfillment infra
    #[msg("FailedOpenbookV2CPI")]
    FailedOpenbookV2CPI,
    /// @deprecated removed with spot fulfillment infra
    #[msg("InvalidOpenbookV2Program")]
    InvalidOpenbookV2Program,
    /// @deprecated removed with spot fulfillment infra
    #[msg("InvalidOpenbookV2Market")]
    InvalidOpenbookV2Market,
    #[msg("Non zero transfer fee")]
    NonZeroTransferFee,
    #[msg("Liquidation order failed to fill")]
    LiquidationOrderFailedToFill,
    /// Deprecated: kept to preserve error code ordering
    #[msg("Deprecated")]
    DepreciatedPredictionMarketOrder,
    #[msg("Ed25519 Ix must be before place and make SignedMsg order ix")]
    InvalidVerificationIxIndex,
    #[msg("SignedMsg message verificaiton failed")]
    SigVerificationFailed,
    #[msg("Market index mismatched b/w taker and maker SignedMsg order params")]
    MismatchedSignedMsgOrderParamsMarketIndex,
    #[msg("Invalid SignedMsg order param")]
    InvalidSignedMsgOrderParam,
    #[msg("Place and take order success condition failed")]
    PlaceAndTakeOrderSuccessConditionFailed,
    /// Deprecated: kept to preserve error code ordering
    #[msg("Deprecated")]
    DeprecatedHighLeverageModeConfig,
    #[msg("Invalid RFQ User Account")]
    InvalidRFQUserAccount,
    #[msg("RFQUserAccount should be mutable")]
    RFQUserAccountWrongMutability,
    #[msg("RFQUserAccount has too many active RFQs")]
    RFQUserAccountFull,
    #[msg("RFQ order not filled as expected")]
    RFQOrderNotFilled,
    #[msg("RFQ orders must be jit makers")]
    InvalidRFQOrder,
    #[msg("RFQ matches must be valid")]
    InvalidRFQMatch,
    #[msg("Invalid SignedMsg user account")]
    InvalidSignedMsgUserAccount,
    #[msg("SignedMsg account wrong mutability")]
    SignedMsgUserAccountWrongMutability,
    #[msg("SignedMsgUserAccount has too many active orders")]
    SignedMsgUserOrdersAccountFull,
    // --- index 300 (error 0x18FC / 6300) ---
    /// @deprecated Nothing raises it since `place_and_make_signed_msg_perp_order` was removed.
    #[msg("Order with SignedMsg uuid does not exist")]
    SignedMsgOrderDoesNotExist,
    #[msg("SignedMsg order id cannot be 0s")]
    InvalidSignedMsgOrderId,
    #[msg("Invalid pool id")]
    InvalidPoolId,
    /// @deprecated protected maker mode removed
    #[msg("Invalid Protected Maker Mode Config")]
    InvalidProtectedMakerModeConfig,
    #[msg("Invalid pyth lazer storage owner")]
    InvalidPythLazerStorageOwner,
    #[msg("Verification of pyth lazer message failed")]
    UnverifiedPythLazerMessage,
    #[msg("Invalid pyth lazer message")]
    InvalidPythLazerMessage,
    #[msg("Pyth lazer message does not correspond to correct fed id")]
    PythLazerMessagePriceFeedMismatch,
    #[msg("InvalidLiquidateSpotWithSwap")]
    InvalidLiquidateSpotWithSwap,
    #[msg("User in SignedMsg message does not match user in ix context")]
    SignedMsgUserContextUserMismatch,
    /// Deprecated: kept to preserve error code ordering
    #[msg("Deprecated")]
    Deprecated1,
    /// Deprecated: kept to preserve error code ordering
    #[msg("Deprecated")]
    Deprecated2,
    #[msg("Invalid Transfer Perp Position")]
    InvalidTransferPerpPosition,
    #[msg("Invalid SignedMsgUserOrders resize")]
    InvalidSignedMsgUserOrdersResize,
    /// Deprecated: kept to preserve error code ordering
    #[msg("Deprecated")]
    DeprecatedCouldNotDeserializeHighLeverageModeConfig,
    #[msg("Invalid If Rebalance Config")]
    /// @deprecated if-rebalance machinery removed; code preserved (ABI-stable enum)
    InvalidIfRebalanceConfig,
    #[msg("Invalid If Rebalance Swap")]
    /// @deprecated if-rebalance machinery removed; code preserved (ABI-stable enum)
    InvalidIfRebalanceSwap,
    #[msg("Invalid RevenueShare resize")]
    InvalidRevenueShareResize,
    #[msg("Builder has been revoked")]
    BuilderRevoked,
    #[msg("Builder fee is greater than max fee bps")]
    InvalidBuilderFee,
    #[msg("RevenueShareEscrow authority mismatch")]
    RevenueShareEscrowAuthorityMismatch,
    #[msg("RevenueShareEscrow has too many active orders")]
    RevenueShareEscrowOrdersAccountFull,
    #[msg("Invalid RevenueShareAccount")]
    InvalidRevenueShareAccount,
    #[msg("Cannot revoke builder with open orders")]
    CannotRevokeBuilderWithOpenOrders,
    #[msg("Unable to load builder account")]
    UnableToLoadRevenueShareAccount,
    #[msg("Invalid Constituent")]
    InvalidConstituent,
    #[msg("Invalid Amm Constituent Mapping argument")]
    InvalidAmmConstituentMappingArgument,
    #[msg("Constituent not found")]
    ConstituentNotFound,
    #[msg("Constituent could not load")]
    ConstituentCouldNotLoad,
    #[msg("Constituent wrong mutability")]
    ConstituentWrongMutability,
    #[msg("Wrong number of constituents passed to instruction")]
    WrongNumberOfConstituents,
    #[msg("Insufficient constituent token balance")]
    InsufficientConstituentTokenBalance,
    #[msg("Amm Cache data too stale")]
    AMMCacheStale,
    #[msg("LP Pool AUM not updated recently")]
    LpPoolAumDelayed,
    #[msg("Constituent oracle is stale")]
    ConstituentOracleStale,
    #[msg("LP Invariant failed")]
    LpInvariantFailed,
    #[msg("Invalid constituent derivative weights")]
    InvalidConstituentDerivativeWeights,
    #[msg("Max DLP AUM Breached")]
    MaxDlpAumBreached,
    #[msg("Settle Lp Pool Disabled")]
    SettleLpPoolDisabled,
    #[msg("Mint/Redeem Lp Pool Disabled")]
    MintRedeemLpPoolDisabled,
    #[msg("Settlement amount exceeded")]
    LpPoolSettleInvariantBreached,
    #[msg("Invalid constituent operation")]
    InvalidConstituentOperation,
    #[msg("Unauthorized for operation")]
    Unauthorized,
    #[msg("Invalid Lp Pool Id for Operation")]
    InvalidLpPoolId,
    #[msg("MarketIndexNotFoundAmmCache")]
    MarketIndexNotFoundAmmCache,
    #[msg("Invalid Isolated Perp Market")]
    InvalidIsolatedPerpMarket,
    #[msg("Invalid scale order count - must be between 2 and 10")]
    InvalidOrderScaleOrderCount,
    #[msg("Invalid scale order price range")]
    InvalidOrderScalePriceRange,
    #[msg("Invalid perp market config")]
    InvalidPerpMarketConfig,
    // --- index 349 (error 0x192D / 6349) --- last variant
    #[msg("Insurance fund withdrawal recipient must be the designated treasury address")]
    InvalidInsuranceFundWithdrawalRecipient,
    #[msg("Spot DLOB trading is disabled")]
    SpotDlobTradingDisabled,
    #[msg("Signer is not authorized for this admin tier")]
    InvalidAdminTier,
    #[msg("Withdraw guard threshold notional exceeds max")]
    WithdrawGuardThresholdNotionalTooLarge,
    #[msg("Recipient must be the configured protocol fee recipient")]
    InvalidProtocolFeeRecipient,
    #[msg("Insufficient protocol fees available to withdraw")]
    InsufficientProtocolFees,
    #[msg("Native dispatch: supplied state account is not the canonical Velocity state PDA")]
    InvalidNativeStateAccount,
    #[msg("Native dispatch: supplied market account is not a Velocity perp market")]
    InvalidNativePerpMarketAccount,
    #[msg("Isolated positions are not enabled in this build")]
    IsolatedPositionDisabled,
    #[msg("Account equity is below the user-set equity floor")]
    EquityBelowFloor,
    #[msg("Invalid equity floor transfer between subaccounts")]
    InvalidEquityFloorTransfer,
    #[msg("Insurance fund deposit would mint zero shares")]
    IFDepositMintsZeroShares,
    #[msg("Liquidation would worsen the account's margin shortage")]
    LiquidationWorsensAccountHealth,
    #[msg("Perp bankruptcies must be resolved before spot bankruptcies")]
    PerpBankruptcyMustPrecedeSpot,
    #[msg("Revenue share recipient user must be sub_account_id 0")]
    InvalidRevenueShareRecipient,
    #[msg("Spot market daily deposit limit hit")]
    DailyDepositLimit,
    #[msg("The name 'USDT' is reserved for the quote spot market (index 0)")]
    ReservedSpotMarketName,
    #[msg("Cannot modify a builder-coded order; cancel and re-place instead")]
    CannotModifyBuilderOrder,
    #[msg("Invalid account extension")]
    InvalidAccountExtension,
    #[msg("Invalid equity breaker reset")]
    InvalidEquityBreakerReset,
    #[msg("Native dispatch: instruction data is malformed for this opcode")]
    InvalidNativeInstructionData,
    #[msg("MM oracle updates are disabled by the admin feature-bit kill switch")]
    MmOracleUpdateDisabled,
    #[msg("Spot market interest is too stale to value a borrow for margin")]
    SpotMarketInterestStaleForMargin,
    #[msg("Market still owes builder/referrer revenue share; settle it before delisting")]
    UnsettledRevenueShareOnDelist,
    #[msg("Revenue share order can still be paid; settle it instead of forfeiting")]
    RevenueShareOrderNotForfeitable,
    #[msg("vAMM quote management value is outside the hot role bounds")]
    VammQuoteManagementValueOutOfBounds,
    #[msg("Quoter registry entry config is invalid")]
    InvalidQuoterConfig,
    #[msg("Signer does not control this quoter registry entry")]
    InvalidQuoterAuthority,
    #[msg("CLOB crank condition account cannot cover the keeper payment")]
    InsufficientCrankReservoir,
    #[msg("Order is placed on the CLOB; cancel it there (cancel_order_v1)")]
    OrderPlacedOnClob,
    #[msg("Trigger is awaiting a price recross after eviction")]
    OrderAwaitingTriggerRecross,
    #[msg("Cross match legs are imbalanced")]
    CrossMatchImbalanced,
    #[msg("Cross match is not profitable after fees")]
    CrossMatchUnprofitable,
    #[msg("Faster-than-default activation requires the flow-authority attestation")]
    UnattestedFastActivation,
    #[msg("Quoter returned a malformed quote/execute response")]
    InvalidQuoterResponse,
    #[msg("Quoter filled more base than the router allocated to it")]
    QuoterOverfilled,
    #[msg("Quoter filled at a price its quote does not support")]
    QuoterFillOffQuote,
    #[msg("Quoter returned a balance change for a user it may not act against")]
    QuoterSubjectNotPermitted,
    #[msg("More loaded users than the quoter wire can carry")]
    TooManyQuoterWireUsers,
    #[msg("Claimed route does not match the one the order was signed with")]
    SignedRouteMismatch,
    #[msg("A quoter the order's signed route names is absent from the fill")]
    SignedRouteEntryMissing,
    /// @deprecated The cross crank no longer refuses a book by predicate. A
    /// crossing remainder claims its cover, and claimed depth is outside the
    /// matchable set of every caller that does not consume reservations.
    #[msg("A crossed taker remainder must be resolved by crank_taker_origin_cross")]
    CrossedTakerRemainderPending,
    #[msg("No resolvable taker-origin cross on this book")]
    NoTakerOriginCross,
    #[msg("Crossing would leave the taker worse off than its resting price")]
    TakerOriginCrossWorseForTaker,
    #[msg("Crank treasury has too few lamports for this payout")]
    InsufficientCrankTreasury,
    #[msg("Crank reservoir is above its refill watermark")]
    CrankReservoirNotLow,
    #[msg("User conditions sync does not cover every market the user is exposed in")]
    InvalidUserConditionsSync,
    #[msg("A book withheld depth and the transaction had room to carry its owner")]
    FillerOmittedReachableMaker,
    #[msg("A book withheld depth and the transaction carries a loaded user that filled nothing")]
    FillerPaddedTheUserSet,
    #[msg("A book withheld depth and the fill cannot count the transaction's accounts")]
    FillerObligationUncountable,
    #[msg("A quoter filled less base than the allocation it won from its own quote")]
    QuoterFilledShort,
    #[msg("A book withheld depth and the transaction carries a quoter outside the signed route")]
    FillerCarriedUnroutedQuoter,
    /// @deprecated A reduce-only maker order rests on the CLOB. No path produces this.
    #[msg("A reduce-only order cannot rest on the CLOB; the book cannot clamp its fill to the position")]
    ReduceOnlyOrderCannotRestOnClob,
    #[msg(
        "User has orders resting on the CLOB; force_cancel_clob_orders must run before liquidation"
    )]
    LiquidationConflictsWithClobOrders,
    #[msg(
        "A quoter reported more base or more retired orders than velocity reserved for that user"
    )]
    QuoterReportExceedsReservation,
    #[msg(
        "The book runs an activation speed bump; an unattested taker rests on the book instead of filling synchronously"
    )]
    UnattestedSynchronousTake,
    #[msg("The market's quoter slab has no vacant slot")]
    QuoterSlabFull,
    #[msg("The market's quoter slab holds no approved copy of this entry")]
    QuoterNotOnSlab,
    #[msg("Cross match sold below the price its buy leg paid")]
    CrossMatchLegsDoNotCross,
    #[msg("Only the protocol user may skip the taker checks of a fill")]
    TakerExposureNotProtocolOwned,
    #[msg("The market's book cannot rest a fired trigger, so the trigger stays armed")]
    ClobRestUnavailable,
    #[msg("A user order slot holds only a trigger order")]
    OrderTypeNotConditional,
    #[msg("A quoter CPI's arguments could not be sized or serialized")]
    PropAmmArgsEncodeFailed,
    #[msg("An account the quoter entry names is absent from the fill's accounts")]
    QuoterCpiAccountMissing,
    #[msg("A quoter CPI's arguments exceed the byte cap the wire allows")]
    QuoterCpiArgsTooLarge,
    #[msg("The quoter's response account is already borrowed")]
    PropAmmResponseAccountBorrowConflict,
    #[msg("A fill consults more quoters than the route allows")]
    TooManyQuotersConsulted,
    #[msg("A relay condition block failed its layout, version or bounds check")]
    InvalidConditionBlock,
    #[msg("A resolver list exceeds the condition block's region")]
    ConditionResolverListTooLarge,
    #[msg("The router quote buffer holds no more sources")]
    RouterQuoteSourcesFull,
    #[msg("A source's book holds no more levels")]
    RouterQuoteLevelsFull,
    #[msg("A router quote row names no source")]
    RouterQuoteRowWithoutSource,
    #[msg("A resolved crank does not fit the relay scratch region")]
    RelayScratchStageFailed,
    #[msg("A program-keeper crank requires the market's conditions account")]
    CrankConditionsAccountRequired,
    #[msg("The conditions account is for a different market than the fired order")]
    CrankConditionsMarketMismatch,
    #[msg("The perp market account is for a different market than the caller names")]
    PerpMarketAccountMismatch,
    #[msg("Force cancel received more order references than it allows")]
    TooManyForceCancelRefs,
    #[msg("The order rested on a different side than the caller declared")]
    ForceCancelSideMismatch,
    #[msg("A cross names the same account as more than one participant")]
    CrossParticipantOverlap,
    #[msg("The market's CLOB quoter is not active and approved")]
    ClobQuoterNotActive,
    #[msg("The fired condition is not a crank velocity serves")]
    UnrecognizedCrankCondition,
    #[msg("The router executor holds no quoter slab")]
    QuoterExecutorMissingSlab,
    #[msg("The router executor has no quoter at the index the route names")]
    QuoterExecutorIndexOutOfRange,
    #[msg("A CPI to a quoter program failed")]
    FailedQuoterCpi,
    #[msg("The fill omits the market's mandatory public book")]
    RequiredBaselineQuoterOmitted,
    #[msg("A router split needs at least one book")]
    QuoterRouteHasNoBooks,
    #[msg("The AMM quoter refresh requires the market maker oracle")]
    AmmQuoterMissingMmOracle,
    #[msg("The crank conditions account is too small for the reservoir mirror")]
    ClobCrankAccountTooSmall,
    #[msg("The escrow order belongs to a different market")]
    RevenueShareOrderMarketMismatch,
    #[msg("The escrow order accrued no fees to forfeit")]
    RevenueShareOrderHasNoFeesAccrued,
    #[msg("The caller passed more markets than the instruction allows")]
    TooManyMarketsPassed,
    #[msg("The spot market is not active")]
    SpotMarketNotActive,
    #[msg("A delegate cannot transfer a deposit")]
    DelegateTransferNotAllowed,
    #[msg("The builder codes feature is disabled")]
    BuilderCodesDisabled,
    #[msg("The perp market is not quoted in the given quote spot market")]
    PerpMarketQuoteSpotMismatch,
    #[msg("A revenue share escrow needs at least one order slot")]
    RevenueShareEscrowNeedsOrderSlot,
    #[msg("The staged relay executor is malformed")]
    RelayExecutorInvalid,
    #[msg("The self-sync price is above the cost ceiling")]
    SelfSyncCostAboveCeiling,
    #[msg("The self-sync interval is above the slot ceiling")]
    SelfSyncIntervalAboveCeiling,
    #[msg("The resolver needs the stored margin map accounts")]
    ResolverMarginMapMissing,
    #[msg("The crank treasury watermark or refill target is out of range")]
    CrankTreasuryWatermarkInvalid,
    #[msg("A resolver account must be read only")]
    ResolverAccountMustBeReadOnly,
    #[msg("The feature gate account has the wrong data length")]
    InvalidFeatureGateAccount,
    #[msg("The feature gate is not activated yet")]
    FeatureGateNotActive,
    #[msg("A slot duration sync cannot regress the active duration")]
    SlotDurationSyncRegresses,
    #[msg("The slot duration transition is already recorded or not effective")]
    SlotDurationTransitionInvalid,
    #[msg("An oracle staleness window is out of range")]
    InvalidOracleStalenessWindow,
    #[msg("The authority is not a whitelisted external depositor")]
    ExternalDepositorNotWhitelisted,
    #[msg("The spot market vault invariant is intact, so there is nothing to settle")]
    SpotMarketVaultInvariantNotViolated,
    #[msg("The self-sync interval is too short for the payment it carries")]
    SelfSyncIntervalTooShort,
    #[msg("A signed-message entry that neither fills nor rests places nothing of its bundle")]
    SignedMsgEntryNeitherFilledNorRested,
    #[msg("The market has no CLOB, so no crank can fire a trigger order on it")]
    TriggerMarketHasNoClob,
}

#[macro_export]
macro_rules! print_error {
    ($err:expr) => {{
        || {
            let error_code: ErrorCode = $err;
            msg!("{:?} thrown at {}:{}", error_code, file!(), line!());
            $err
        }
    }};
}

#[macro_export]
macro_rules! math_error {
    () => {{
        || {
            let error_code = $crate::error::ErrorCode::MathError;
            msg!("Error {} thrown at {}:{}", error_code, file!(), line!());
            error_code
        }
    }};
}
