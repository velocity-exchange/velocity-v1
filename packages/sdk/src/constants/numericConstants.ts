import { LAMPORTS_PER_SOL } from '@solana/web3.js';
import { BN } from '../isomorphic/anchor';
import { millis, millisFromSecs } from '../math/time';

/** Precision constants used throughout the SDK. Each mirrors an on-chain fixed-point scale — a raw `BN` amount at that precision represents `amount / 10^exponent` in human units (e.g. `PRICE_PRECISION` = 1e6, so a raw price of `1_500_000` is `$1.50`). Values must stay numerically identical to the Rust program's `math::constants` — a mismatch here silently mis-scales every derived SDK computation. */
export const ZERO = new BN(0);
export const ONE = new BN(1);
export const TWO = new BN(2);
export const THREE = new BN(3);
export const FOUR = new BN(4);
export const FIVE = new BN(5);
export const SIX = new BN(6);
export const SEVEN = new BN(7);
export const EIGHT = new BN(8);
export const NINE = new BN(9);
export const TEN = new BN(10);
export const TEN_THOUSAND = new BN(10000);
/** Largest value safely representable as a JS `number` (2^53 - 1), wrapped in a `BN`; used as a practical "infinite" ceiling, not an on-chain limit. */
export const BN_MAX = new BN(Number.MAX_SAFE_INTEGER);
export const TEN_MILLION = TEN_THOUSAND.mul(TEN_THOUSAND);

/**
 * Percent of the referee's taker fee that an accelerated referrer receives.
 * The configured Standard rate does not change this value.
 */
export const ACCELERATED_REFERRER_REWARD_PERCENT = 20;

/** Default max leverage (5x) used by SDK helpers when a market's actual `marginRatioInitial` isn't available. */
export const MAX_LEVERAGE = new BN(5);
/** `u64::MAX`; sentinel order size meaning "close the whole position" in reduce-only market-order helpers. */
export const MAX_LEVERAGE_ORDER_SIZE = new BN('18446744073709551615');

/** Exponent for `PERCENTAGE_PRECISION` (1e6). */
export const PERCENTAGE_PRECISION_EXP = new BN(6);
/** 1e6; precision for percentage/fraction fields (e.g. AMM concentration coefficient, funding ramp slope, LP pool volatility). */
export const PERCENTAGE_PRECISION = new BN(10).pow(PERCENTAGE_PRECISION_EXP);

/**
 * Minimum wall-clock time between two accepted MM-oracle writes. An MM-oracle price
 * without `oracleSlotDelayOverride` also uses it as the immediate-fill staleness bound.
 */
export const MM_ORACLE_MIN_WRITE_GAP = millis(800);
/**
 * Max wall-clock age between an MM-oracle update's source observation slot and the slot it lands. The
 * program skips an update outside the bound either way, and asserts this value is at or below `MM_ORACLE_MIN_WRITE_GAP`.
 */
export const MM_ORACLE_MAX_SOURCE_AGE = millis(800);
/** Alias of `PERCENTAGE_PRECISION` (1e6) for the AMM's `concentrationCoef` field. */
export const CONCENTRATION_PRECISION = PERCENTAGE_PRECISION;

/** Exponent for `QUOTE_PRECISION` (1e6). */
export const QUOTE_PRECISION_EXP = new BN(6);
/** Exponent for `FUNDING_RATE_BUFFER_PRECISION` (1e3) — the extra precision funding rates carry beyond `PRICE_PRECISION`. */
export const FUNDING_RATE_BUFFER_PRECISION_EXP = new BN(3);
/** Exponent for `PRICE_PRECISION` (1e6). */
/**
 * A market order that names no price fills at most `oracle / this` away from
 * the oracle. 200 is 0.5 percent. Mirrors the program's
 * `DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION`.
 */
export const DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION = new BN(200);

export const PRICE_PRECISION_EXP = new BN(6);
/** Exponent for `FUNDING_RATE_PRECISION` (1e9) = `PRICE_PRECISION_EXP` + `FUNDING_RATE_BUFFER_PRECISION_EXP`. */
export const FUNDING_RATE_PRECISION_EXP = PRICE_PRECISION_EXP.add(
	FUNDING_RATE_BUFFER_PRECISION_EXP
);
/** Exponent for `PEG_PRECISION` (1e6). */
export const PEG_PRECISION_EXP = new BN(6);
/** Exponent for `AMM_RESERVE_PRECISION` / `BASE_PRECISION` (1e9). */
export const AMM_RESERVE_PRECISION_EXP = new BN(9);

/** Exponent for `SPOT_MARKET_RATE_PRECISION` (1e6). */
export const SPOT_MARKET_RATE_PRECISION_EXP = new BN(6);
/** 1e6; precision for spot market interest-rate fields (`optimalBorrowRate`, `maxBorrowRate`). */
export const SPOT_MARKET_RATE_PRECISION = new BN(10).pow(
	SPOT_MARKET_RATE_PRECISION_EXP
);

/** Exponent for `SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION` (1e10). */
export const SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION_EXP = new BN(10);
/** 1e10; precision for `SpotMarketAccount.cumulativeDepositInterest`/`cumulativeBorrowInterest`. */
export const SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION = new BN(10).pow(
	SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION_EXP
);

/** Exponent for `SPOT_MARKET_UTILIZATION_PRECISION` (1e6). */
export const SPOT_MARKET_UTILIZATION_PRECISION_EXP = new BN(6);
/** 1e6; precision for spot market utilization fields (`optimalUtilization`, `utilizationTwap`). */
export const SPOT_MARKET_UTILIZATION_PRECISION = new BN(10).pow(
	SPOT_MARKET_UTILIZATION_PRECISION_EXP
);

/** 1e4; precision for spot asset/liability weight fields (`initialAssetWeight`, `maintenanceLiabilityWeight`, etc). Same scale as `MARGIN_PRECISION`. */
export const SPOT_MARKET_WEIGHT_PRECISION = new BN(10000);
/** Exponent for `SPOT_MARKET_BALANCE_PRECISION` (1e9). */
export const SPOT_MARKET_BALANCE_PRECISION_EXP = new BN(9);
/** 1e9; precision for scaled spot balances (`SpotPosition.scaledBalance`, `PoolBalance.scaledBalance`, `depositBalance`/`borrowBalance`). Multiply by the market's cumulative interest to get the token amount. */
export const SPOT_MARKET_BALANCE_PRECISION = new BN(10).pow(
	SPOT_MARKET_BALANCE_PRECISION_EXP
);
/** Exponent for `SPOT_MARKET_IMF_PRECISION` (1e6). */
export const SPOT_MARKET_IMF_PRECISION_EXP = new BN(6);

/** 1e6; precision for `imfFactor`-style fields (deprecated alias — most `imfFactor` fields use `MARGIN_PRECISION`, 1e4). */
export const SPOT_MARKET_IMF_PRECISION = new BN(10).pow(
	SPOT_MARKET_IMF_PRECISION_EXP
);
/** 1e6; precision for `liquidatorFee`/`ifLiquidationFee`/`protocolLiquidationFee` fields on perp and spot markets. */
export const LIQUIDATION_FEE_PRECISION = new BN(1000000);

/** 1e6; precision for quote-asset (USD-denominated) amounts — pnl, deposits/withdraws, fees, collateral. The protocol's most widely used precision. */
export const QUOTE_PRECISION = new BN(10).pow(QUOTE_PRECISION_EXP);
/** 1e6; precision for prices (`Order.price`, oracle prices, TWAPs, `oraclePriceOffset`). */
export const PRICE_PRECISION = new BN(10).pow(PRICE_PRECISION_EXP);
/** 1e9; precision for funding-rate fields (`cumulativeFundingRateLong/Short`, `lastFundingRate`, `FundingRateRecord.fundingRate`). Unit is quote per base. */
export const FUNDING_RATE_PRECISION = new BN(10).pow(
	FUNDING_RATE_PRECISION_EXP
);
/** 1e3; the extra scale factor between `PRICE_PRECISION` and `FUNDING_RATE_PRECISION`. */
export const FUNDING_RATE_BUFFER_PRECISION = new BN(10).pow(
	FUNDING_RATE_BUFFER_PRECISION_EXP
);
/** 1e6; precision for `AMM.pegMultiplier`, normalizing the AMM's quote reserve. */
export const PEG_PRECISION = new BN(10).pow(PEG_PRECISION_EXP);

/** 1e9; precision for AMM constant-product reserves (`baseAssetReserve`, `quoteAssetReserve`, `sqrtK`, etc). Same scale as `BASE_PRECISION`. */
export const AMM_RESERVE_PRECISION = new BN(10).pow(AMM_RESERVE_PRECISION_EXP);

/** 1e9; precision for perp base-asset amounts (position size, `Order.baseAssetAmount`, open interest). Alias of `AMM_RESERVE_PRECISION`. */
export const BASE_PRECISION = AMM_RESERVE_PRECISION;
export const BASE_PRECISION_EXP = AMM_RESERVE_PRECISION_EXP;

/** 1e3; ratio to convert an `AMM_RESERVE_PRECISION` amount into `QUOTE_PRECISION` units (at peg = 1). */
export const AMM_TO_QUOTE_PRECISION_RATIO =
	AMM_RESERVE_PRECISION.div(QUOTE_PRECISION); // 10^3
/** 1e1 (=10); ratio between `PRICE_PRECISION` and `PEG_PRECISION`. */
export const PRICE_DIV_PEG = PRICE_PRECISION.div(PEG_PRECISION); //10^1
/** 1e1 (=10); ratio between `PRICE_PRECISION` and `QUOTE_PRECISION`. */
export const PRICE_TO_QUOTE_PRECISION = PRICE_PRECISION.div(QUOTE_PRECISION); // 10^1
/** 1e9; combined `AMM_RESERVE_PRECISION * PEG_PRECISION / QUOTE_PRECISION` conversion ratio used in AMM quote-value math. */
export const AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO =
	AMM_RESERVE_PRECISION.mul(PEG_PRECISION).div(QUOTE_PRECISION); // 10^9
/** 1e4; precision for margin-ratio fields (`marginRatioInitial`, `marginRatioMaintenance`, `imfFactor`, `maxMarginRatio`, `StateAccount.liquidationMarginBufferRatio`). */
export const MARGIN_PRECISION = TEN_THOUSAND;
/** 1e4; precision for basis-point fields where 1 unit = 1 bp (e.g. `PerpMarketAccount.fundingClampThreshold`). */
export const BPS_PRECISION = TEN_THOUSAND; // 1 unit = 1bp
/** 1e6; precision for AMM bid/ask spread fields (`baseSpread`, `maxSpread`, `longSpread`, `shortSpread`, `lastOracleReservePriceSpreadPct`). */
export const BID_ASK_SPREAD_PRECISION = new BN(1000000); // 10^6
/**
 * `PERCENTAGE_PRECISION / 400`, which is 2500 ppm, or 25 bp. Oracle confidence at or above this carries
 * full weight in the vol spread. Below it the weight ramps continuously from `1 / SPREAD_CONF_DISCOUNT_DIVISOR`.
 */
export const SPREAD_CONF_FULL_WEIGHT_THRESHOLD = PERCENTAGE_PRECISION.div(
	new BN(400)
);

/** Denominator of the confidence contribution's starting weight below `SPREAD_CONF_FULL_WEIGHT_THRESHOLD`. */
export const SPREAD_CONF_DISCOUNT_DIVISOR = new BN(20);

/** 1e4; precision for `StateAccount.initialPctToLiquidate`, the fraction of a position liquidated per partial-liquidation pass. */
export const LIQUIDATION_PCT_PRECISION = TEN_THOUSAND;
/** Denominator (3333) used to derive `FUNDING_RATE_OFFSET_PERCENTAGE`; yields ~10.95% annualized when applied hourly. */
export const FUNDING_RATE_OFFSET_DENOMINATOR = new BN(3333);
/** `FUNDING_RATE_PRECISION / FUNDING_RATE_OFFSET_DENOMINATOR`; the default funding-rate offset floor/ceiling nudge. */
export const FUNDING_RATE_OFFSET_PERCENTAGE = FUNDING_RATE_PRECISION.div(
	FUNDING_RATE_OFFSET_DENOMINATOR
);
/** Denominator (2000, i.e. 0.05%) used to clamp per-update funding-rate changes. */
export const FUNDING_RATE_CLAMP_DENOMINATOR = new BN(2000);
/** `PRICE_PRECISION * AMM_TO_QUOTE_PRECISION_RATIO`; combined conversion ratio used when pricing AMM reserve amounts directly in quote terms. */
export const PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO = PRICE_PRECISION.mul(
	AMM_TO_QUOTE_PRECISION_RATIO
);

/** 60 seconds. */
export const ONE_MINUTE = new BN(60);
/** 300 seconds. */
export const FIVE_MINUTE = new BN(60 * 5);
/** Max age of the last fill before the trigger price's last-fill leg is treated as absent (oracle price substitutes). */
export const TRIGGER_PRICE_LAST_FILL_MAX_AGE = FIVE_MINUTE;
/** 3600 seconds. */
export const ONE_HOUR = new BN(60 * 60);
/** 31,536,000 seconds (365 days). */
export const ONE_YEAR = new BN(31536000);

/**
 * How many funding periods the mark TWAP may stay unwritten. Past this many periods the program discards
 * the stored value and re-seeds it from the oracle TWAP. Callers floor the window at {@link ONE_HOUR}, because a market's funding period can be zero.
 */
export const MARK_TWAP_RESEED_FUNDING_PERIODS = new BN(3);

/**
 * The largest share of a spot borrow that un-booked interest may hide, in `PERCENTAGE_PRECISION`. One basis point.
 * Withdraw, transfer, swap and a perp fill reject with `SpotMarketInterestStaleForMargin` past this share. Use `maxSpotInterestStalenessForMargin` for one market's time window.
 */
export const MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN =
	PERCENTAGE_PRECISION.divn(10_000);

/** Ceiling on the window `maxSpotInterestStalenessForMargin` derives, so a low-rate market cannot go un-cranked indefinitely. */
export const MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN = ONE_HOUR;

/** Market index of the protocol's quote spot market (the protocol's quote asset on mainnet). */
export const QUOTE_SPOT_MARKET_INDEX = 0;

/** 1e9 (`LAMPORTS_PER_SOL`); token-mint precision for wrapped SOL spot markets. */
export const LAMPORTS_PRECISION = new BN(LAMPORTS_PER_SOL);
/** 9; decimal exponent for `LAMPORTS_PRECISION`. */
export const LAMPORTS_EXP = new BN(Math.log10(LAMPORTS_PER_SOL));

/** `QUOTE_PRECISION / 100` = $0.01; per-open-order margin requirement reserved against free collateral. */
export const OPEN_ORDER_MARGIN_REQUIREMENT = QUOTE_PRECISION.div(new BN(100));

/** -$25 (`QUOTE_PRECISION`); default floor for `AMM.netRevenueSinceLastFunding` below which the funding-rate spread retreats, damping the AMM from over-widening its spread after a large one-off loss. */
export const DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT = new BN(
	-25
).mul(QUOTE_PRECISION);

/** 13 days in seconds; minimum account age before an idle user account becomes eligible for keeper-initiated deletion. */
export const ACCOUNT_AGE_DELETION_CUTOFF_SECONDS = 60 * 60 * 24 * 13; // 13 days
/**
 * Inactivity time in milliseconds after which a keeper can mark a user account idle. This one-hour
 * threshold applies to an account with equity under $1,000. Every other account waits one week.
 */
export const IDLE_TIME = millisFromSecs(3_600);
/**
 * @deprecated Solana slot time is no longer a constant. Feature gates move it from 400 down to
 * 200ms. Resolve the live value with `currentSlotDuration(client, currentSlot)` from `math/time.ts`.
 */
export const SLOT_TIME_ESTIMATE_MS = 400;

/** `QUOTE_PRECISION / 100` = $0.01; a perp position smaller than this is treated as dust (safe to ignore/close for free). */
export const DUST_POSITION_SIZE = QUOTE_PRECISION.divn(100); // Dust position is any position smaller than 1c

/**
 * $100 (`QUOTE_PRECISION`); the cap on a user's unrealized positive perp PnL that may count toward
 * *initial* margin (free collateral for opening/increasing positions). Unrealized profit beyond
 * this amount is not double-counted as collateral for new risk — it still counts in full for
 * maintenance-margin (liquidation) calculations. Mirrors the Rust
 * `MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN` constant; keep both in sync.
 */
export const MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN = new BN(100).mul(
	QUOTE_PRECISION
); // max upnl for initial margin calc

/** Max number of pubkeys per `getMultipleAccounts` RPC call the SDK will batch (RPC-imposed ceiling is 100; kept at 99 for headroom). */
export const GET_MULTIPLE_ACCOUNTS_CHUNK_SIZE = 99;

/**
 * The standing bankruptcy first-loss tranche the fee sweep leaves in `feeLedger.pendingIfFee`. 10 bps of open-interest notional, in `PERCENTAGE_PRECISION`.
 * A market whose `bankruptcyIfFloorPct` reads 0 uses this value, which covers every market written before that field existed.
 */
export const DEFAULT_BANKRUPTCY_IF_FLOOR_PCT = 1000;
/**
 * The `PerpMarketAccount.bankruptcyIfFloorPct` value that turns the standing floor off. The sentinel is needed because 0 means `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT`.
 * A latched bankruptcy still freezes the sweep through `pendingBankruptcyClaims`.
 */
export const BANKRUPTCY_IF_FLOOR_DISABLED = 4294967295; // u32::MAX

// integer constants
// Built with `BN` directly (not `BigNum.fromPrint`) to avoid a module-load
// circular dependency: `bigNum.ts` imports `ZERO` from this file, so importing
// `bigNum.ts` first leaves `BigNum` undefined while this module body evaluates.
/** `i64::MAX` (9223372036854775807). */
export const MAX_I64 = new BN('9223372036854775807');
/** `i64::MIN` (-9223372036854775808). */
export const MIN_I64 = new BN('-9223372036854775808');

/**
 * Trailing 30-day volume breakpoint between the Regular tier and VIP 1, in
 * `QUOTE_PRECISION`.
 */
export const VIP_FEE_TIER_ONE_VOLUME_QUOTE = new BN(5_000_000).mul(
	QUOTE_PRECISION
);

/**
 * Trailing 30-day volume breakpoint between VIP 1 and VIP 2, in `QUOTE_PRECISION`.
 */
export const VIP_FEE_TIER_TWO_VOLUME_QUOTE = new BN(80_000_000).mul(
	QUOTE_PRECISION
);

/**
 * Trailing 30-day volume breakpoint between VIP 2 and VIP 3, in `QUOTE_PRECISION`.
 */
export const VIP_FEE_TIER_THREE_VOLUME_QUOTE = new BN(200_000_000).mul(
	QUOTE_PRECISION
);
