import {
	BN,
	User,
	PublicKey,
	PRICE_PRECISION,
	QUOTE_PRECISION,
	SPOT_MARKET_BALANCE_PRECISION,
	SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION,
	SpotBalanceType,
} from '../../src';
import { mockPerpMarkets, mockSpotMarkets } from '../dlob/helpers';
import { assert } from '../../src/assert/assert';
import {
	mockUserAccount as baseMockUserAccount,
	makeMockUser,
} from './helpers';
import * as _ from 'lodash';

const SOL_MARKET_INDEX = 1;
const SOL_ORACLE_PRICE = 100;

// 10 SOL deposit/borrow priced against a $100 oracle, with the market's stored
// 5min TWAP set to `twap5min` and `lastOraclePriceTwapTs` left at 0 (i.e. maximally
// stale — the case where a live-projected TWAP collapses back onto the oracle price)
async function makeStrictSpotUser(
	twap5min: number,
	balanceType: SpotBalanceType
): Promise<User> {
	const myMockPerpMarkets = _.cloneDeep(mockPerpMarkets);
	const myMockSpotMarkets = _.cloneDeep(mockSpotMarkets);
	const myMockUserAccount = _.cloneDeep(baseMockUserAccount);

	const solMarket = myMockSpotMarkets[SOL_MARKET_INDEX];
	// distinct oracle per market: all mock markets otherwise share the
	// default pubkey, which would collapse their prices onto one entry
	solMarket.oracle = new PublicKey(11);
	solMarket.initialAssetWeight = 8000;
	solMarket.initialLiabilityWeight = 12000;
	solMarket.cumulativeDepositInterest =
		SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION;
	solMarket.cumulativeBorrowInterest =
		SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION;
	solMarket.historicalOracleData.lastOraclePriceTwap5Min = new BN(
		twap5min * PRICE_PRECISION.toNumber()
	);

	const spotPosition = myMockUserAccount.spotPositions[SOL_MARKET_INDEX];
	spotPosition.marketIndex = SOL_MARKET_INDEX;
	spotPosition.balanceType = balanceType;
	spotPosition.scaledBalance = new BN(10).mul(SPOT_MARKET_BALANCE_PRECISION);

	const spotOraclePrices = myMockSpotMarkets.map(() => 1);
	spotOraclePrices[SOL_MARKET_INDEX] = SOL_ORACLE_PRICE;

	return makeMockUser(
		myMockPerpMarkets,
		myMockSpotMarkets,
		myMockUserAccount,
		myMockPerpMarkets.map(() => 1),
		spotOraclePrices
	);
}

describe('strict spot pricing uses the stored 5min oracle twap', () => {
	it('values a deposit at min(oracle, stored twap)', async () => {
		const user = await makeStrictSpotUser(90, SpotBalanceType.DEPOSIT);

		// 10 SOL at the stored $90 twap, not the live $100 oracle
		const strict = user.getSpotMarketAssetAndLiabilityValue(
			SOL_MARKET_INDEX,
			undefined,
			undefined,
			false,
			true
		);
		const expectedStrict = new BN(900).mul(QUOTE_PRECISION);
		assert(
			strict.totalAssetValue.eq(expectedStrict),
			`strict deposit value mismatch: ${strict.totalAssetValue.toString()} != ${expectedStrict.toString()}`
		);

		const loose = user.getSpotMarketAssetAndLiabilityValue(
			SOL_MARKET_INDEX,
			undefined,
			undefined,
			false,
			false
		);
		const expectedLoose = new BN(1000).mul(QUOTE_PRECISION);
		assert(
			loose.totalAssetValue.eq(expectedLoose),
			`non-strict deposit value mismatch: ${loose.totalAssetValue.toString()} != ${expectedLoose.toString()}`
		);
	});

	it('values a borrow at max(oracle, stored twap)', async () => {
		const user = await makeStrictSpotUser(110, SpotBalanceType.BORROW);

		// 10 SOL at the stored $110 twap, not the live $100 oracle
		const strict = user.getSpotMarketAssetAndLiabilityValue(
			SOL_MARKET_INDEX,
			undefined,
			undefined,
			false,
			true
		);
		const expectedStrict = new BN(1100).mul(QUOTE_PRECISION);
		assert(
			strict.totalLiabilityValue.eq(expectedStrict),
			`strict borrow value mismatch: ${strict.totalLiabilityValue.toString()} != ${expectedStrict.toString()}`
		);

		const loose = user.getSpotMarketAssetAndLiabilityValue(
			SOL_MARKET_INDEX,
			undefined,
			undefined,
			false,
			false
		);
		const expectedLoose = new BN(1000).mul(QUOTE_PRECISION);
		assert(
			loose.totalLiabilityValue.eq(expectedLoose),
			`non-strict borrow value mismatch: ${loose.totalLiabilityValue.toString()} != ${expectedLoose.toString()}`
		);
	});

	it('getMarginCalculation weights a deposit off the stored twap', async () => {
		const user = await makeStrictSpotUser(90, SpotBalanceType.DEPOSIT);

		// $900 strict value * 0.8 initial asset weight
		const strict = user.getMarginCalculation('Initial', { strict: true });
		const expectedStrict = new BN(720).mul(QUOTE_PRECISION);
		assert(
			strict.totalCollateral.eq(expectedStrict),
			`strict total collateral mismatch: ${strict.totalCollateral.toString()} != ${expectedStrict.toString()}`
		);

		const loose = user.getMarginCalculation('Initial', { strict: false });
		const expectedLoose = new BN(800).mul(QUOTE_PRECISION);
		assert(
			loose.totalCollateral.eq(expectedLoose),
			`non-strict total collateral mismatch: ${loose.totalCollateral.toString()} != ${expectedLoose.toString()}`
		);
	});

	it('getMarginCalculation weights a borrow off the stored twap', async () => {
		const user = await makeStrictSpotUser(110, SpotBalanceType.BORROW);

		// $1100 strict value * 1.2 initial liability weight
		const strict = user.getMarginCalculation('Initial', { strict: true });
		const expectedStrict = new BN(1320).mul(QUOTE_PRECISION);
		assert(
			strict.marginRequirement.eq(expectedStrict),
			`strict margin requirement mismatch: ${strict.marginRequirement.toString()} != ${expectedStrict.toString()}`
		);

		const loose = user.getMarginCalculation('Initial', { strict: false });
		const expectedLoose = new BN(1200).mul(QUOTE_PRECISION);
		assert(
			loose.marginRequirement.eq(expectedLoose),
			`non-strict margin requirement mismatch: ${loose.marginRequirement.toString()} != ${expectedLoose.toString()}`
		);
	});
});
