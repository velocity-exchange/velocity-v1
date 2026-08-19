import { assert } from 'chai';
import _ from 'lodash';
import {
	PerpMarketAccount,
	PositionFlag,
	SpotBalanceType,
	SpotMarketAccount,
} from '../../src/types';
import {
	BASE_PRECISION,
	QUOTE_PRECISION,
	SPOT_MARKET_BALANCE_PRECISION,
	ZERO,
} from '../../src/constants/numericConstants';
import { BN } from '../../src';
import { getTokenAmount } from '../../src/math/spotBalance';
import { mockPerpMarkets, mockSpotMarkets } from '../dlob/helpers';
import {
	mockUserAccount as baseMockUserAccount,
	makeMockUser,
} from './helpers';
import {
	isUserBankrupt,
	isIsolatedPositionBankrupt,
	hasIsolatedMarginBankrupt,
} from '../../src/math/bankruptcy';

/**
 * `isUserBankrupt` reads market state (deposit index, PnL pool) and not just the user
 * account, so the value-aware vetoes can only be exercised by varying the markets too.
 */
async function makeUserWithMarkets(
	account,
	mutateMarkets: (
		perpMarkets: Array<PerpMarketAccount>,
		spotMarkets: Array<SpotMarketAccount>
	) => void
) {
	const perpMarkets = _.cloneDeep(mockPerpMarkets);
	const spotMarkets = _.cloneDeep(mockSpotMarkets);
	mutateMarkets(perpMarkets, spotMarkets);
	return await makeMockUser(
		perpMarkets,
		spotMarkets,
		account,
		[1, 1, 1, 1, 1, 1, 1, 1],
		[1, 1, 1, 1, 1, 1, 1, 1]
	);
}

async function makeUserWithAccount(account) {
	return await makeUserWithMarkets(account, () => {});
}

describe('isUserBankrupt', () => {
	it('cross-bankrupt user is still bankrupt when an open isolated position is present', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		// cross position: no base, negative quote (liability), no open orders -> cross-bankrupt shape
		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(-100).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = 0;

		// isolated position: still open (nonzero base) - would have tripped the old
		// unconditional loop into reporting "not bankrupt"
		account.perpPositions[1].marketIndex = 1;
		account.perpPositions[1].baseAssetAmount = new BN(5).mul(BASE_PRECISION);
		account.perpPositions[1].quoteAssetAmount = new BN(0);
		account.perpPositions[1].positionFlag = PositionFlag.IsolatedPosition;

		const user = await makeUserWithAccount(account);

		assert.equal(isUserBankrupt(user), true);
	});

	it('user with no liability is not bankrupt', async () => {
		const account = _.cloneDeep(baseMockUserAccount);
		const user = await makeUserWithAccount(account);
		assert.equal(isUserBankrupt(user), false);
	});

	// OtterSec #151. A keeper that vetoes on the row rather than its value never sends the
	// resolver for these accounts, and the bad-debt repair stalls exactly as it did on chain
	// — ordinary liquidation cannot unstick it either, since `liquidate_spot` rejects a zero
	// token amount before it reaches the bankruptcy-admission check.
	it('deposit row left worthless by socialization does not block bankruptcy', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		// a fully socialized market floors cumulativeDepositInterest at 1, leaving each
		// wiped depositor a positive scaled row worth zero tokens
		account.spotPositions[0].marketIndex = 1;
		account.spotPositions[0].balanceType = SpotBalanceType.DEPOSIT;
		account.spotPositions[0].scaledBalance = SPOT_MARKET_BALANCE_PRECISION;

		// the unrelated borrow whose repair the worthless row was blocking
		account.spotPositions[1].marketIndex = 2;
		account.spotPositions[1].balanceType = SpotBalanceType.BORROW;
		account.spotPositions[1].scaledBalance = SPOT_MARKET_BALANCE_PRECISION;

		const user = await makeUserWithMarkets(account, (_perp, spot) => {
			spot[1].cumulativeDepositInterest = new BN(1);
		});

		// the fixture only proves anything while the row really is worth nothing
		assert.isTrue(
			getTokenAmount(
				account.spotPositions[0].scaledBalance,
				user.velocityClient.getSpotMarketAccountOrThrow(1),
				SpotBalanceType.DEPOSIT
			).eq(ZERO),
			'fixture must leave the deposit row worth zero tokens'
		);

		assert.equal(isUserBankrupt(user), true);
	});

	it('deposit worth at least one token still blocks bankruptcy', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		// same shape as above, but the market was never socialized, so the row is real
		// collateral and must be seized by ordinary liquidation first
		account.spotPositions[0].marketIndex = 1;
		account.spotPositions[0].balanceType = SpotBalanceType.DEPOSIT;
		account.spotPositions[0].scaledBalance = SPOT_MARKET_BALANCE_PRECISION;

		account.spotPositions[1].marketIndex = 2;
		account.spotPositions[1].balanceType = SpotBalanceType.BORROW;
		account.spotPositions[1].scaledBalance = SPOT_MARKET_BALANCE_PRECISION;

		const user = await makeUserWithAccount(account);

		assert.isTrue(
			getTokenAmount(
				account.spotPositions[0].scaledBalance,
				user.velocityClient.getSpotMarketAccountOrThrow(1),
				SpotBalanceType.DEPOSIT
			).gt(ZERO)
		);

		assert.equal(isUserBankrupt(user), false);
	});

	// OtterSec #145. A positive perp quote no longer vetoes unconditionally: while its
	// market's PnL pool is empty the claim can never be settled into a deposit, so treating
	// it as an asset strands a real loss in another market forever.
	it('unfundable perp claim does not block bankruptcy', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		// claim on market 1, whose pool cannot pay any of it
		account.perpPositions[0].marketIndex = 1;
		account.perpPositions[0].baseAssetAmount = ZERO;
		account.perpPositions[0].quoteAssetAmount = new BN(200).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = 0;

		// larger debt on market 2, so the estate is net insolvent
		account.perpPositions[1].marketIndex = 2;
		account.perpPositions[1].baseAssetAmount = ZERO;
		account.perpPositions[1].quoteAssetAmount = new BN(-500).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[1].positionFlag = 0;

		const user = await makeUserWithMarkets(account, (perp) => {
			perp[1].pnlPool.scaledBalance = ZERO;
		});

		assert.equal(isUserBankrupt(user), true);
	});

	/**
	 * An estate holding `aggregateClaims` of claim on a market with `poolDollars` of pnl pool, against
	 * a larger debt in another market.
	 */
	function claimEstateWithPool(aggregateClaims: number, poolDollars: number) {
		const account = _.cloneDeep(baseMockUserAccount);

		account.perpPositions[0].marketIndex = 1;
		account.perpPositions[0].baseAssetAmount = ZERO;
		account.perpPositions[0].quoteAssetAmount = new BN(aggregateClaims).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = 0;

		account.perpPositions[1].marketIndex = 2;
		account.perpPositions[1].baseAssetAmount = ZERO;
		account.perpPositions[1].quoteAssetAmount = new BN(-500).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[1].positionFlag = 0;

		return makeUserWithMarkets(account, (perp) => {
			perp[1].quoteAssetAmount = new BN(aggregateClaims).mul(QUOTE_PRECISION);
			perp[1].pnlPool.scaledBalance = new BN(poolDollars).mul(
				SPOT_MARKET_BALANCE_PRECISION
			);
		});
	}

	it('pool state never blocks bankruptcy', async () => {
		// Empty, part-funded, and funded past the claim: the answer is the same. The resolvers
		// recover what the pool can pay and forfeit the rest, so no pool state can hold the repair
		// open — and trading fees flow into the pool, so a pool-shaped veto would be re-armable by
		// any market participant with a trade.
		for (const pool of [0, 100, 400]) {
			const user = await claimEstateWithPool(200, pool);
			assert.equal(isUserBankrupt(user), true, `pool = ${pool}`);
		}
	});

	it('net solvent estate is not bankrupt however unfundable its claims', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		// $5000 of genuine positive PnL against $1000 of debt: unpayability alone must not
		// admit this account, or the resolver forfeits the whole claim to cover a fraction
		account.perpPositions[0].marketIndex = 1;
		account.perpPositions[0].baseAssetAmount = ZERO;
		account.perpPositions[0].quoteAssetAmount = new BN(5000).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = 0;

		account.perpPositions[1].marketIndex = 2;
		account.perpPositions[1].baseAssetAmount = ZERO;
		account.perpPositions[1].quoteAssetAmount = new BN(-1000).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[1].positionFlag = 0;

		const user = await makeUserWithMarkets(account, (perp) => {
			perp[1].pnlPool.scaledBalance = ZERO;
		});

		assert.equal(isUserBankrupt(user), false);
	});
});

describe('isIsolatedPositionBankrupt', () => {
	it('isolated position with no deposit, no base, and a quote liability is bankrupt', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(-50).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = PositionFlag.IsolatedPosition;
		account.perpPositions[0].isolatedPositionScaledBalance = new BN(0);

		const user = await makeUserWithAccount(account);

		assert.equal(isIsolatedPositionBankrupt(user, 0), true);
	});

	it('isolated position with a remaining deposit is not bankrupt', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(-50).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = PositionFlag.IsolatedPosition;
		account.perpPositions[0].isolatedPositionScaledBalance = new BN(1000);

		const user = await makeUserWithAccount(account);

		assert.equal(isIsolatedPositionBankrupt(user, 0), false);
	});

	it('throws (InvalidPerpPosition) on a non-isolated position', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(-50).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = 0; // not isolated
		account.perpPositions[0].isolatedPositionScaledBalance = new BN(0);

		const user = await makeUserWithAccount(account);

		assert.throws(() => isIsolatedPositionBankrupt(user, 0), /not an isolated/);
	});
});

describe('hasIsolatedMarginBankrupt', () => {
	it('detects an economically-bankrupt isolated position (flag not yet set on-chain)', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		// isolated: drained deposit, flat base, quote liability, no open orders,
		// but the on-chain Bankrupt status flag has NOT been set yet.
		account.perpPositions[0].marketIndex = 1;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(-25).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = PositionFlag.IsolatedPosition;
		account.perpPositions[0].isolatedPositionScaledBalance = new BN(0);

		const user = await makeUserWithAccount(account);

		// isUserBankrupt (cross) deliberately skips isolated positions, and
		// user.isBankrupt() only reads UserStatus.BANKRUPT -> both miss this.
		assert.equal(isUserBankrupt(user), false);
		assert.equal(user.isBankrupt(), false);
		assert.equal(hasIsolatedMarginBankrupt(user), true);
	});

	it('detects an isolated position already flagged Bankrupt on-chain', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		// still has collateral / non-flat, so not economically bankrupt by shape,
		// but the program already set the Bankrupt position flag.
		account.perpPositions[0].marketIndex = 1;
		account.perpPositions[0].baseAssetAmount = new BN(3).mul(BASE_PRECISION);
		account.perpPositions[0].quoteAssetAmount = new BN(0);
		account.perpPositions[0].isolatedPositionScaledBalance = new BN(1000);
		account.perpPositions[0].positionFlag =
			PositionFlag.IsolatedPosition | PositionFlag.Bankruptcy;

		const user = await makeUserWithAccount(account);

		assert.equal(hasIsolatedMarginBankrupt(user), true);
	});

	it('returns false for a healthy isolated position', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		account.perpPositions[0].marketIndex = 1;
		account.perpPositions[0].baseAssetAmount = new BN(3).mul(BASE_PRECISION);
		account.perpPositions[0].quoteAssetAmount = new BN(0);
		account.perpPositions[0].positionFlag = PositionFlag.IsolatedPosition;
		account.perpPositions[0].isolatedPositionScaledBalance = new BN(1000);

		const user = await makeUserWithAccount(account);

		assert.equal(hasIsolatedMarginBankrupt(user), false);
	});
});
