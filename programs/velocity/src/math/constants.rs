use {crate::math::time::Millis, solana_program::native_token::LAMPORTS_PER_SOL}; // expo 9
pub const LAMPORTS_PER_SOL_U64: u64 = LAMPORTS_PER_SOL;
pub const LAMPORTS_PER_SOL_I64: i64 = LAMPORTS_PER_SOL as i64;

// SPOT MARKET CONSTANTS
pub const QUOTE_SPOT_MARKET_INDEX: u16 = 0;

// USER ACCOUNT CONSTANTS
pub const MAX_SPOT_POSITIONS: u8 = 8;
pub const MAX_PERP_POSITIONS: u8 = 8;
pub const MAX_OPEN_ORDERS: u8 = 32;

// PRECISIONS
pub const AMM_RESERVE_PRECISION: u128 = 1_000_000_000; //expo = -9;
pub const AMM_RESERVE_PRECISION_I128: i128 = (AMM_RESERVE_PRECISION) as i128;
pub const BASE_PRECISION: u128 = AMM_RESERVE_PRECISION; //expo = -9;
pub const BASE_PRECISION_I128: i128 = AMM_RESERVE_PRECISION_I128;
pub const BASE_PRECISION_U64: u64 = AMM_RESERVE_PRECISION as u64; //expo = -9;
pub const BASE_PRECISION_I64: i64 = AMM_RESERVE_PRECISION_I128 as i64; //expo = -9;
pub const PERP_DECIMALS: u32 = 9;

pub const PRICE_PRECISION: u128 = 1_000_000; //expo = -6;
pub const PRICE_PRECISION_I128: i128 = PRICE_PRECISION as i128;
pub const PRICE_PRECISION_U64: u64 = 1_000_000; //expo = -6;
pub const PRICE_PRECISION_I64: i64 = 1_000_000; //expo = -6;

pub const PEG_PRECISION: u128 = 1_000_000; //expo = -6
pub const PEG_PRECISION_I128: i128 = PEG_PRECISION as i128; //expo = -6

pub const QUOTE_PRECISION: u128 = 1_000_000; // expo = -6
pub const QUOTE_PRECISION_I128: i128 = 1_000_000; // expo = -6
pub const QUOTE_PRECISION_I64: i64 = 1_000_000; // expo = -6
pub const QUOTE_PRECISION_U64: u64 = 1_000_000; // expo = -6

pub const FUNDING_RATE_BUFFER: u128 = 1_000; // expo = -3
pub const FUNDING_RATE_BUFFER_I128: i128 = FUNDING_RATE_BUFFER as i128; // expo = -3

pub const MARGIN_PRECISION: u32 = 10_000; // expo = -4
pub const MARGIN_PRECISION_U128: u128 = 10_000; // expo = -4
pub const MARGIN_PRECISION_I128: i128 = 10_000; // expo = -4
pub const SPOT_WEIGHT_PRECISION: u32 = MARGIN_PRECISION; // expo = -4
pub const SPOT_WEIGHT_PRECISION_U128: u128 = SPOT_WEIGHT_PRECISION as u128; // expo = -4
pub const SPOT_WEIGHT_PRECISION_I128: i128 = SPOT_WEIGHT_PRECISION as i128; // expo = -4
pub const BPS_PRECISION: u32 = 10_000; // expo = -4, 1 unit = 1bp

pub const LIQUIDATION_PCT_PRECISION: u128 = 10_000;

pub const SPOT_BALANCE_PRECISION: u128 = 1_000_000_000; // expo = -9
pub const SPOT_BALANCE_PRECISION_U64: u64 = 1_000_000_000; // expo = -9
pub const SPOT_CUMULATIVE_INTEREST_PRECISION: u128 = 10_000_000_000; // expo = -10

pub const PERCENTAGE_PRECISION: u128 = 1_000_000; // expo -6 (represents 100%)
pub const PERCENTAGE_PRECISION_I128: i128 = PERCENTAGE_PRECISION as i128;
pub const PERCENTAGE_PRECISION_U64: u64 = PERCENTAGE_PRECISION as u64;
pub const PERCENTAGE_PRECISION_I64: i64 = PERCENTAGE_PRECISION as i64;
pub const PERCENTAGE_PRECISION_I32: i32 = PERCENTAGE_PRECISION as i32;
pub const PERCENTAGE_PRECISION_U32: u32 = PERCENTAGE_PRECISION as u32;

pub const TEN_BPS: i128 = PERCENTAGE_PRECISION_I128 / 1000;
pub const TEN_BPS_I64: i64 = TEN_BPS as i64;
pub const TWO_PT_TWO_PCT: i128 = 22_000;

pub const BID_ASK_SPREAD_PRECISION: u64 = PERCENTAGE_PRECISION as u64; // expo = -6
pub const BID_ASK_SPREAD_PRECISION_I64: i64 = (BID_ASK_SPREAD_PRECISION) as i64;
pub const BID_ASK_SPREAD_PRECISION_U128: u128 = BID_ASK_SPREAD_PRECISION as u128; // expo = -6
pub const BID_ASK_SPREAD_PRECISION_I128: i128 = BID_ASK_SPREAD_PRECISION as i128; // expo = -6

pub const CONCENTRATION_PRECISION: u128 = PERCENTAGE_PRECISION; // expo 6
pub const IF_FACTOR_PRECISION: u128 = PERCENTAGE_PRECISION; // expo 6

pub const SPOT_UTILIZATION_PRECISION: u128 = PERCENTAGE_PRECISION; // expo = -6
pub const SPOT_UTILIZATION_PRECISION_U32: u32 = PERCENTAGE_PRECISION as u32; // expo = -6
pub const SPOT_RATE_PRECISION: u128 = PERCENTAGE_PRECISION; // expo = -6
pub const SPOT_RATE_PRECISION_U32: u32 = PERCENTAGE_PRECISION as u32; // expo = -6
pub const LIQUIDATION_FEE_PRECISION: u32 = PERCENTAGE_PRECISION as u32; // expo = -6
pub const LIQUIDATION_FEE_PRECISION_U128: u128 = LIQUIDATION_FEE_PRECISION as u128; // expo = -6
pub const SPOT_IMF_PRECISION: u32 = PERCENTAGE_PRECISION as u32; // expo = -6
pub const SPOT_IMF_PRECISION_U128: u128 = SPOT_IMF_PRECISION as u128; // expo = -6

// FORMULAIC REPEG / K
pub const K_BPS_UPDATE_SCALE: i128 = PERCENTAGE_PRECISION_I128;
pub const PEG_BPS_UPDATE_SCALE: u128 = PERCENTAGE_PRECISION; // expo = -6 (represents 100%)

// PRECISION CONVERSIONS
pub const PRICE_TO_PEG_PRECISION_RATIO: u128 = PRICE_PRECISION / PEG_PRECISION; // expo: 1 (Delete if we keep peg/price as 1e6)
pub const AMM_TO_QUOTE_PRECISION_RATIO: u128 = AMM_RESERVE_PRECISION / QUOTE_PRECISION; // expo: 3
pub const AMM_TO_QUOTE_PRECISION_RATIO_I128: i128 =
    (AMM_RESERVE_PRECISION / QUOTE_PRECISION) as i128; // expo: 3
pub const AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO: u128 =
    AMM_RESERVE_PRECISION * PEG_PRECISION / QUOTE_PRECISION; // expo: 9
pub const QUOTE_TO_BASE_AMT_FUNDING_PRECISION: i128 =
    AMM_RESERVE_PRECISION_I128 * FUNDING_RATE_PRECISION_I128 / QUOTE_PRECISION_I128; // expo: 12
pub const PRICE_TO_QUOTE_PRECISION_RATIO: u128 = PRICE_PRECISION / QUOTE_PRECISION; // expo: 1
pub const PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO: u128 =
    PRICE_PRECISION * AMM_TO_QUOTE_PRECISION_RATIO; // expo 9
pub const LIQUIDATION_FEE_TO_MARGIN_PRECISION_RATIO: u32 = // expo 2
    LIQUIDATION_FEE_PRECISION / MARGIN_PRECISION;
pub const LIQUIDATION_FEE_TO_MARGIN_PRECISION_RATIO_U128: u128 = // expo 2
    LIQUIDATION_FEE_TO_MARGIN_PRECISION_RATIO as u128;
pub const FUNDING_RATE_TO_QUOTE_PRECISION_PRECISION_RATIO: u128 = // expo 3
    FUNDING_RATE_PRECISION / QUOTE_PRECISION;
pub const FUNDING_RATE_PRECISION: u128 = PRICE_PRECISION * FUNDING_RATE_BUFFER; // expo: 9
pub const FUNDING_RATE_PRECISION_I128: i128 = PRICE_PRECISION_I128 * FUNDING_RATE_BUFFER_I128; // expo: 9
pub const FUNDING_RATE_PRECISION_I64: i64 = FUNDING_RATE_PRECISION_I128 as i64; // expo: 9

pub const AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO_I128: i128 =
    AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO as i128;
pub const PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO_I128: i128 =
    PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO as i128; // expo 9

// FEE REBATES
pub const SHARE_OF_IF_ESCROW_ALLOCATED_TO_PROTOCOL_NUMERATOR: u128 = 1;
pub const SHARE_OF_IF_ESCROW_ALLOCATED_TO_PROTOCOL_DENOMINATOR: u128 = 2;

pub const SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_NUMERATOR: u128 = 1;
pub const SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_DENOMINATOR: u128 = 1;

// TIME PERIODS
pub const ONE_MINUTE: i128 = 60_i128;
pub const FIVE_MINUTE: i128 = (60 * 5) as i128;
pub const ONE_HOUR: i64 = 3600;
pub const ONE_HOUR_I128: i128 = ONE_HOUR as i128;
pub const TWENTY_FOUR_HOUR: i64 = 3600 * 24;
pub const THIRTEEN_DAY: i64 = TWENTY_FOUR_HOUR * 13; // IF unstake default

/// The largest share of a spot borrow that un-booked interest may hide before the
/// borrow can no longer be valued for margin on a value-releasing path
/// (OtterSec #135 / #148).
///
/// Margin values a scaled borrow through the market's stored
/// `cumulative_borrow_interest`. Interest accrued since `last_interest_ts` is left
/// out, so the debt is understated by `debt x borrow_rate x elapsed / year`. The
/// bound is on that understated share, not on the elapsed time.
/// `validate_borrow_rate` constrains `max_borrow_rate` only against
/// `optimal_borrow_rate`, so the rate has no ceiling. One fixed time window would
/// hide an arbitrary share on a high-rate market.
///
/// One basis point is far inside the gap between initial and maintenance margin. It
/// is too small to build bad debt with. An un-cranked market drifts arbitrarily far,
/// which is the real vector.
/// `math::margin::max_spot_interest_staleness_for_margin` turns this share into the
/// per-market time window that enforces it.
pub const MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN: u128 = PERCENTAGE_PRECISION / 10_000;

/// Ceiling on the window `math::margin::max_spot_interest_staleness_for_margin`
/// derives, so a low-rate market cannot go un-cranked indefinitely.
///
/// A market that charges little interest earns a wide window from
/// `MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN` alone. The admin can raise its rate
/// at any time, and the raise applies to the whole un-booked interval. The window a
/// low rate earns is therefore not a promise about that interval.
///
/// Recovery needs no special privileges.
/// `update_spot_market_cumulative_interest` is permissionless and can be bundled
/// into the same transaction.
pub const MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN: i64 = ONE_HOUR;
pub const EPOCH_DURATION: i64 = TWENTY_FOUR_HOUR * 28;
pub const THIRTY_DAY: i64 = TWENTY_FOUR_HOUR * 30;
pub const THIRTY_DAY_I128: i128 = (TWENTY_FOUR_HOUR * 30) as i128;
pub const ONE_YEAR: u128 = 31536000;

/// How many funding periods the mark TWAP may stay unwritten. Past this many periods
/// `MarketStats::reseed_mark_twap_from_oracle_if_stale` discards the stored value and
/// re-seeds it from the oracle TWAP. That function documents why.
///
/// The multiplier must stay above 2. A market whose only writer is the funding crank
/// writes once per funding period in the steady state. `on_the_hour_update` can also
/// stretch one legitimate interval to about 1.67 periods. A lower bound re-seeds a
/// market that is only quiet or cranked late, and discards a real premium.
pub const MARK_TWAP_RESEED_FUNDING_PERIODS: i64 = 3;
/// Max age of the last fill before the trigger price's last-fill leg is
/// treated as absent (oracle price substitutes).
pub const TRIGGER_PRICE_LAST_FILL_MAX_AGE: i64 = FIVE_MINUTE as i64;

// QUOTE AMOUNTS
pub const TWO_HUNDRED_MILLION_QUOTE: u64 = 200_000_000_u64 * QUOTE_PRECISION_U64;
pub const ONE_HUNDRED_MILLION_QUOTE: u64 = 100_000_000_u64 * QUOTE_PRECISION_U64;
pub const FIFTY_MILLION_QUOTE: u64 = 50_000_000_u64 * QUOTE_PRECISION_U64;
pub const TEN_MILLION_QUOTE: u64 = 10_000_000_u64 * QUOTE_PRECISION_U64;
pub const FIVE_MILLION_QUOTE: u64 = 5_000_000_u64 * QUOTE_PRECISION_U64;
pub const ONE_MILLION_QUOTE: u64 = 1_000_000_u64 * QUOTE_PRECISION_U64;
pub const TWO_HUNDRED_FIFTY_THOUSAND_QUOTE: u64 = 250_000_u64 * QUOTE_PRECISION_U64;
pub const ONE_HUNDRED_THOUSAND_QUOTE: u64 = 100_000_u64 * QUOTE_PRECISION_U64;
pub const TWENTY_FIVE_THOUSAND_QUOTE: u64 = 25_000_u64 * QUOTE_PRECISION_U64;
pub const TEN_THOUSAND_QUOTE: u64 = 10_000_u64 * QUOTE_PRECISION_U64;
pub const ONE_THOUSAND_QUOTE: u64 = 1_000_u64 * QUOTE_PRECISION_U64;
pub const TWO_HUNDRED_FIFTY_QUOTE: u64 = 250_u64 * QUOTE_PRECISION_U64;

// INSURANCE TIERS
pub const INSURANCE_A_MAX: u64 = ONE_HUNDRED_MILLION_QUOTE;
pub const INSURANCE_B_MAX: u64 = ONE_MILLION_QUOTE;
pub const INSURANCE_C_MAX: u64 = ONE_HUNDRED_THOUSAND_QUOTE;
pub const INSURANCE_SPECULATIVE_MAX: u64 = 0;

// QUOTE THRESHOLDS
pub const FEE_POOL_TO_REVENUE_POOL_THRESHOLD: u128 = TWO_HUNDRED_FIFTY_QUOTE as u128;

// FEES
pub const ONE_BPS_DENOMINATOR: u32 = 10000;
pub const LP_FEE_SLICE_NUMERATOR: u128 = 8;
pub const LP_FEE_SLICE_DENOMINATOR: u128 = 10;
pub const FEE_DENOMINATOR: u32 = 10 * ONE_BPS_DENOMINATOR;
pub const FEE_PERCENTAGE_DENOMINATOR: u32 = 100;
pub const ACCELERATED_REFERRER_REWARD_NUMERATOR: u32 = 20;
/// While true, three paths permanently grant Accelerated referral status: user
/// initialization, a perp fill for either the taker or the maker, and a completed swap.
/// Set it to false to stop new enrollment. There is no runtime switch, so that takes a
/// program upgrade.
///
/// Enrollment is temporary. When it ends, delete this constant and the branches that
/// read it. Delete `AcceleratedReferralStatus::AutoEnrollmentBlocked` and
/// `AcceleratedReferralStatusChange::AutoEnrollment` too, which exist only for automatic
/// enrollment. `UserStats::accelerated_referral_status` and
/// `update_user_accelerated_referral_status` stay.
pub const ACCELERATED_REFERRAL_ENROLLMENT_ENABLED: bool = true;
/// Global ceiling on a builder-code fee, in tenths of a bps. The fee is
/// `quote * fee_tenth_bps / FEE_DENOMINATOR`, so `FEE_DENOMINATOR` is 100% and 1000 is
/// 1%. TUNABLE.
///
/// A builder's own `max_fee_tenth_bps` is set at approval with no ceiling, up to
/// `u16::MAX`, about 65.5% of notional. This caps the fee actually charged, so the
/// builder-fee rail cannot move value a taker could not withdraw under initial margin
/// (OtterSec #83).
pub const MAX_BUILDER_FEE_TENTH_BPS: u16 = 1000;
/// Ceiling on the magnitude of `PerpMarket.taker_fee_addon_tenth_bps`, in tenth-bps,
/// so 100 is 10 bps. It keeps the per-market fee add-on within the same order of
/// magnitude as the tier fees it adjusts. TUNABLE.
pub const MAX_TAKER_FEE_ADDON_TENTH_BPS: u16 = 100;
/// Highest populated perp fee-tier index. Tiers `0` through this index are live and
/// the remaining `fee_tiers` slots are zeroed spares. `determine_perp_fee_tier` clamps
/// its result to this index. `update_promo_fee_tier` validates against it, so a promo
/// floor cannot pass validation and then mean a lower tier. Move it together with the
/// schedule in `FeeStructure::perps_default`.
pub const PERP_FEE_TIER_MAX_INDEX: usize = 3;
pub const OPEN_ORDER_MARGIN_REQUIREMENT: u128 = QUOTE_PRECISION / 100;
/// Max oracle-value loss a strictly reducing `end_swap` may realize while the account
/// is under equity-floor protection, meaning the floor is set or the breaker is
/// tripped. The output value must be at least the input value minus this many bps. The
/// in leg is valued at the strict max of the live oracle price and the 5min twap, and
/// the out leg at the strict min. This bounds how much value a reducing swap can lose
/// through a bad route while the account is frozen. 100 is 1%. TUNABLE.
pub const EQUITY_FLOOR_SWAP_MAX_VALUE_LOSS_BPS: u128 = 100;
pub const FEE_ADJUSTMENT_MAX: u64 = 100;
pub const FEE_ADJUSTMENT_MAX_I16: i16 = FEE_ADJUSTMENT_MAX as i16;

// PRICE AMOUNTS
pub const HUNDRENTH_OF_CENT: u128 = PRICE_PRECISION / 10_000; //.0001

// CONSTRAINTS
pub const MAX_K_BPS_INCREASE: i128 = TEN_BPS;
pub const MAX_K_BPS_DECREASE: i128 = TWO_PT_TWO_PCT;
pub const MAX_UPDATE_K_PRICE_CHANGE: u128 = HUNDRENTH_OF_CENT;
pub const MAX_SQRT_K: u128 = 1000000000000000000000; // 1e21 (count 'em!)
pub const MAX_BASE_ASSET_AMOUNT_WITH_AMM: u128 = 400000000000000000; // 4e17 (count 'em!)

pub const MAX_PEG_BPS_INCREASE: u128 = TEN_BPS as u128; // 10 bps increase
pub const MAX_PEG_BPS_DECREASE: u128 = TEN_BPS as u128; // 10 bps decrease

pub const MAX_APR_PER_REVENUE_SETTLE_TO_INSURANCE_FUND_VAULT: u128 =
    10 * PERCENTAGE_PRECISION_U64 as u128; // 1000% APR

pub const MAX_CONCENTRATION_COEFFICIENT: u128 = 1_414_200;
pub const MAX_LIQUIDATION_MULTIPLIER: u32 = 3;
/// 0.01 bps per [`crate::math::time::Millis::UNIT`] of elapsed time, which is 400ms.
/// `get_liquidation_fee` counts whole periods before it applies this rate. The period
/// length is the historical calibration. A change to it changes the economics.
pub const LIQUIDATION_FEE_INCREASE_PER_PERIOD: u32 = LIQUIDATION_FEE_PRECISION / 1_000_000;
pub const MAX_LIQUIDATION_SLIPPAGE: i128 = 10_000; // expo = -2
pub const MAX_LIQUIDATION_SLIPPAGE_U128: u128 = 10_000; // expo = -2
pub const MAX_MARK_TWAP_DIVERGENCE: u128 = 500_000; // expo = -3

pub const MAX_MARGIN_RATIO: u32 = MARGIN_PRECISION; // 1x leverage
pub const MIN_MARGIN_RATIO: u32 = 125; // 80x leverage

pub const MAX_BID_ASK_INVENTORY_SKEW_FACTOR: u64 = 10 * BID_ASK_SPREAD_PRECISION;

// SPREAD (vlp/amm/math/spread.rs)
/// Oracle confidence at or above this carries full weight in the vol spread. Below
/// it the contribution weight ramps continuously from 1/20 to full weight. The unit
/// is PERCENTAGE_PRECISION, so the threshold is 25 bp.
pub const SPREAD_CONF_FULL_WEIGHT_THRESHOLD: u64 = PERCENTAGE_PRECISION_U64 / 400;
/// Denominator of the confidence contribution's starting weight below the
/// full-weight threshold.
pub const SPREAD_CONF_DISCOUNT_DIVISOR: u64 = 20;
/// Divisor applied to the market's average std pct when it competes with
/// the confidence for the vol spread base.
pub const SPREAD_VOL_STD_DISCOUNT_DIVISOR: u128 = 4;
/// The revenue retreat is capped at `max_spread` divided by this.
pub const SPREAD_REVENUE_RETREAT_MAX_DIVISOR: u64 = 10;
/// Reference-price-offset sign-transition smoothing: the budget for the
/// pre-division step, calibrated per [`crate::math::time::Millis::UNIT`] of elapsed
/// time, which is 400ms. `compute_quote_state` prorates it by the elapsed
/// milliseconds. The convergence rate per wall-clock second is then the same at
/// every slot duration, and matches the per-slot behavior at 400ms.
pub const REF_PRICE_OFFSET_SMOOTHING_PER_PERIOD_BUDGET: i128 = 1000;
/// Reference-price-offset sign-transition smoothing: the capped delta is
/// divided by this to get the per-refresh step.
pub const REF_PRICE_OFFSET_SMOOTHING_STEP_DIVISOR: i128 = 10;
/// Reference-price-offset sign-transition smoothing: minimum per-refresh
/// step, so a transition always makes progress.
pub const REF_PRICE_OFFSET_SMOOTHING_MIN_STEP: i32 = 10;

/// Maximum percent divergence from oracle price for bids/asks to be included in mark TWAP calculation.
/// Bids more than this % below oracle and asks more than this % above oracle are filtered out.
pub const BID_ASK_TWAP_MAX_ORACLE_DIVERGENCE_PERCENT: u64 = 15;

/// Minimum wall-clock time a quote must rest onchain before it can move the
/// bid/ask/mark TWAP (OtterSec #146).
///
/// `update_perp_bid_ask_twap` samples the book from caller-supplied `User` accounts.
/// Nothing else in the program checks how long an order has existed. A post-only limit
/// order, or any order with `auction_duration == 0`, counts as resting in the slot it
/// was placed. The crank's caller could therefore place a self-crossed pair of quotes,
/// crank, and cancel, all in one transaction. That moves the mark TWAP that
/// `get_perp_baseline_start_price_offset` uses to set a third party's forced-close
/// auction band, at no risk of a fill.
///
/// The value comes from `min_auction_duration = 20`, which `place_perp_order` forces
/// onto every triggered stop-loss auction (`controller/orders`). A quote must rest at
/// least as long as the auction it can move, so a third party could have taken it
/// first. 24 slots is that 20 plus a slack leader window, about 9.6s at 400ms. It stays
/// below the 60s `SafeTriggerOrder` rest window and the 256-slot
/// `Order::posted_slot_tail` modulus.
///
/// The value is wall-clock and the read site converts it to actual slots. The
/// `min_auction_duration` and `SafeTriggerOrder` windows it is calibrated against are
/// wall-clock too, so all three scale together and the resting rule survives every
/// slot-duration gate. The `posted_slot_tail` modulus does not scale, because it is a
/// u8 field width. The honest age window therefore shrinks in wall-clock as slots get
/// faster. The rest requirement is 48 actual slots at 200ms, which still fits under
/// that modulus.
pub const BID_ASK_TWAP_MIN_QUOTE_REST: Millis = Millis::from_ms(9_600);

pub const MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN: i128 = 100 * QUOTE_PRECISION_I128; // max upnl for initial margin calc
pub const DEFAULT_MAX_TWAP_UPDATE_PRICE_BAND_DENOMINATOR: i64 = 3; // '3' here means clamp new data point to 33% (1/3) divergence from current twap (if twap > 0)

// DEFAULTS
pub const DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT: i64 = -25 * QUOTE_PRECISION_I64; //$25 loss
/// Default `PerpMarket.bankruptcy_if_floor_pct` for new markets: 10 bps of
/// open-interest notional retained in `pending_if_fee` as a standing
/// bankruptcy tranche (PERCENTAGE_PRECISION). A market that holds `0` also uses this
/// value, and every market created before the field existed holds `0`. The tranche
/// therefore does not depend on an admin call per market.
pub const DEFAULT_BANKRUPTCY_IF_FLOOR_PCT: u32 = PERCENTAGE_PRECISION_U32 / 1000; // 0.1%
/// The `PerpMarket.bankruptcy_if_floor_pct` value that turns the standing floor off.
/// `0` means use `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT`, so a market with no floor needs an
/// explicit sentinel. This value does not affect the freeze that
/// `pending_bankruptcy_claims` applies.
pub const BANKRUPTCY_IF_FLOOR_DISABLED: u32 = u32::MAX;
pub const DEFAULT_LARGE_BID_ASK_FACTOR: u64 = 10 * BID_ASK_SPREAD_PRECISION;
pub const DEFAULT_LIQUIDATION_MARGIN_BUFFER_RATIO: u32 = MARGIN_PRECISION / 50; // 2%
pub const DEFAULT_BASE_ASSET_AMOUNT_STEP_SIZE: u64 = BASE_PRECISION_U64 / 10000; // 1e-4;
pub const DEFAULT_QUOTE_ASSET_AMOUNT_TICK_SIZE: u64 =
    PRICE_PRECISION_U64 / DEFAULT_BASE_ASSET_AMOUNT_STEP_SIZE; // 1e-2

// FUNDING
pub const FUNDING_RATE_OFFSET_DENOMINATOR: i64 = 3333; // 3333 => 10.95% annualized rate for hourly funding
pub const FUNDING_RATE_OFFSET_PERCENTAGE: i64 =
    FUNDING_RATE_PRECISION_I64 / FUNDING_RATE_OFFSET_DENOMINATOR;
pub const FUNDING_RATE_CLAMP_DENOMINATOR: i64 = 2000; // 2000 => 0.05%

// ORDERS
pub const AUCTION_DERIVE_PRICE_FRACTION: i64 = 200;

// WITHDRAWS
pub const SPOT_MARKET_TOKEN_TWAP_WINDOW: i64 = TWENTY_FOUR_HOUR;
pub const MAX_WITHDRAW_GUARD_THRESHOLD_NOTIONAL: u128 = 10_000 * QUOTE_PRECISION; // $10k

// POOL IDs
pub const LST_POOL_ID: u8 = 2;

// SPOT INTEREST RATE CURVE
pub const INTEREST_RATE_SEGMENT_AND_WEIGHTS: &[(u128, u128)] = &[
    (850_000, 50),
    (900_000, 100),
    (950_000, 150),
    (990_000, 200),
    (995_000, 250),
    (1_000_000, 250),
];

// MM ORACLE
/// Min wall-clock time between accepted writes, 800ms. The write gate and the
/// immediate-fill unset threshold inside `oracle_validity` both express it in actual
/// slots.
pub const MM_ORACLE_MIN_WRITE_GAP: Millis = Millis::from_ms(800);
pub const MM_ORACLE_MAX_STEP_PCT_PRECISION: i128 = PERCENTAGE_PRECISION_I128 / 100; // 1%
/// Max wall-clock age between an MM oracle update's source observation slot, carried
/// in the payload, and the slot it lands. Both directions are bounded. The stored
/// `mm_oracle_slot` is the landing slot. Without this bound a signed update that lands
/// late, and a recent blockhash allows about 150 slots, would make an old observation
/// read as fresh. The future direction guards against a wrong-unit source value that
/// would disable the gate. A skipped write costs nothing, because by the time an update
/// is this late the crank has newer data to send.
///
/// The value must stay at or below `MM_ORACLE_MIN_WRITE_GAP`, which the assertion below
/// enforces. The landing-slot stamp makes `oracle_delay` understate true observation
/// age by up to this bound. The immediate-fill gate's unset threshold of
/// `MM_ORACLE_MIN_WRITE_GAP` then bounds true age to the sum of the two. The write gate
/// ceils and this bound floors, so that sum is `ceil(gap / d) + floor(age / d)` slots.
/// That is exactly twice the gap at 400ms and at 200ms slots, and within one slot of
/// twice the gap at 350ms, 300ms and 250ms. A wider bound here widens what "slot-fresh"
/// means everywhere downstream.
pub const MM_ORACLE_MAX_SOURCE_AGE: Millis = Millis::from_ms(800);
static_assertions::const_assert!(
    MM_ORACLE_MAX_SOURCE_AGE.as_ms() <= MM_ORACLE_MIN_WRITE_GAP.as_ms()
);
