import { expect } from 'chai';
import {
	BN,
	MarketStatus,
	MarketType,
	PublicKey,
	QUOTE_PRECISION,
	RevenueShareEscrowAccount,
	RevenueShareOrder,
	ZERO,
} from '@velocity-exchange/sdk';

import {
	findRevenueShareForfeits,
	findRevenueShareSettles,
	UserPnlSettlerBot,
} from './userPnlSettler';

const HEALTHY_MARKET = 1;
const DEAD_MARKET = 2; // oracle unavailable, e.g. a stale devnet test market

// A negative-PnL perp position. baseAssetAmount == 0 keeps the SDK's
// calculateClaimablePnl on its trivial path (returns quoteAssetAmount), so the
// bot's real settlement logic runs without needing a full AMM/oracle fixture.
function negativePosition(marketIndex: number) {
	return {
		marketIndex,
		baseAssetAmount: ZERO,
		quoteAssetAmount: new BN(-50).mul(QUOTE_PRECISION), // -$50 unsettled
		lpShares: ZERO,
	};
}

function fakeUser(positions: ReturnType<typeof negativePosition>[]) {
	return {
		getUserAccountOrThrow: () => ({ poolId: 0 }),
		getActivePerpPositions: () => positions,
		getUserAccountPublicKey: () => ({ toBase58: () => 'FakeUserPubkey' }),
	};
}

// Minimal UserPnlSettlerBot with just the fields findLargestNegativePnlUsersToSettle
// touches. Built via Object.create to skip the heavy constructor (which spins up
// BlockhashSubscriber/UserMap/RevenueShareEscrowMap).
function makeBot(opts: {
	users: ReturnType<typeof fakeUser>[];
	deadMarkets: Set<number>;
	oracleCalls: { count: number };
}): UserPnlSettlerBot {
	const bot = Object.create(UserPnlSettlerBot.prototype) as any;

	bot.marketIndexes = [];
	bot.minPnlToSettle = new BN(-10).mul(QUOTE_PRECISION); // settle if |pnl| >= $10
	bot.missingOracleWarned = new Set<number>();

	bot.userMap = {
		size: () => opts.users.length,
		values: () => opts.users[Symbol.iterator](),
	};

	bot.velocityClient = {
		getPerpMarketAccount: (_idx: number) => ({ pausedOperations: 0 }),
		getSpotMarketAccount: (_idx: number) => ({}),
		getOracleDataForPerpMarket: (idx: number) => {
			opts.oracleCalls.count++;
			if (opts.deadMarkets.has(idx)) {
				// Mirrors VelocityClient.getOracleDataForPerpMarket when the oracle
				// has no price data.
				throw new Error(`No oracle price data for perp market ${idx}`);
			}
			return { price: new BN(100).mul(QUOTE_PRECISION) };
		},
	};

	return bot as UserPnlSettlerBot;
}

describe('UserPnlSettlerBot missing-oracle resilience', () => {
	it('skips a market whose oracle is missing and still settles the healthy market', async () => {
		const oracleCalls = { count: 0 };
		// Dead market is listed first so the old code would throw before ever
		// reaching the healthy market.
		const user = fakeUser([
			negativePosition(DEAD_MARKET),
			negativePosition(HEALTHY_MARKET),
		]);
		const bot = makeBot({
			users: [user],
			deadMarkets: new Set([DEAD_MARKET]),
			oracleCalls,
		});

		const result = await (bot as any).findLargestNegativePnlUsersToSettle(
			Date.now() / 1000
		);

		// Healthy market still produced a settle candidate...
		expect(result.has(HEALTHY_MARKET)).to.be.true;
		expect(result.get(HEALTHY_MARKET)).to.have.length(1);
		expect(result.get(HEALTHY_MARKET)[0].pnl).to.equal(-50);

		// ...while the dead-oracle market was skipped, not settled or thrown on.
		expect(result.has(DEAD_MARKET)).to.be.false;

		// The dead market's oracle was actually queried (proving we hit the throw
		// path) and recorded so it's only warned about once.
		expect(oracleCalls.count).to.be.greaterThan(0);
		expect((bot as any).missingOracleWarned.has(DEAD_MARKET)).to.be.true;
	});

	it('getOracleDataForPerpMarketSafe swallows the throw and returns undefined', () => {
		const oracleCalls = { count: 0 };
		const bot = makeBot({
			users: [],
			deadMarkets: new Set([DEAD_MARKET]),
			oracleCalls,
		});

		// Healthy market returns data; dead market returns undefined instead of throwing.
		expect((bot as any).getOracleDataForPerpMarketSafe(HEALTHY_MARKET)).to.not
			.be.undefined;
		expect((bot as any).getOracleDataForPerpMarketSafe(DEAD_MARKET)).to.be
			.undefined;

		// Warns/records only once even across repeated calls.
		(bot as any).getOracleDataForPerpMarketSafe(DEAD_MARKET);
		expect((bot as any).missingOracleWarned.size).to.equal(1);
	});
});

function revenueShareRow(marketIndex: number, feesAccrued: number) {
	return {
		marketIndex,
		marketType: MarketType.PERP,
		feesAccrued: new BN(feesAccrued),
	} as unknown as RevenueShareOrder;
}

function escrowWith(rows: RevenueShareOrder[]): RevenueShareEscrowAccount {
	return {
		authority: PublicKey.unique(),
		orders: rows,
	} as unknown as RevenueShareEscrowAccount;
}

describe('revenue share work lists', () => {
	const statuses = new Map<number, MarketStatus>([
		[0, MarketStatus.ACTIVE],
		[1, MarketStatus.SETTLEMENT],
		[2, MarketStatus.DELISTED],
	]);
	const marketStatus = (marketIndex: number) => statuses.get(marketIndex);

	it('settles each owing market once, largest debt first, and skips delisted markets', () => {
		const small = escrowWith([revenueShareRow(0, 5), revenueShareRow(0, 0)]);
		const large = escrowWith([
			revenueShareRow(0, 40),
			revenueShareRow(0, 10),
			revenueShareRow(1, 20),
			revenueShareRow(2, 99),
			revenueShareRow(7, 99),
		]);

		const settles = findRevenueShareSettles([small, large], marketStatus, 10);

		expect(
			settles.map((s) => [s.escrow, s.marketIndex, s.owed.toNumber()])
		).to.deep.equal([
			[large, 0, 50],
			[large, 1, 20],
			[small, 0, 5],
		]);
	});

	it('stops at the limit', () => {
		const escrows = [0, 1, 2].map(() => escrowWith([revenueShareRow(0, 1)]));

		expect(findRevenueShareSettles(escrows, marketStatus, 2)).to.have.length(2);
		expect(findRevenueShareSettles(escrows, marketStatus, -1)).to.have.length(
			0
		);
	});

	it('forfeits only owing rows on a market in settlement', () => {
		const escrow = escrowWith([
			revenueShareRow(0, 10),
			revenueShareRow(1, 10),
			revenueShareRow(1, 0),
			revenueShareRow(1, 3),
		]);

		const forfeits = findRevenueShareForfeits([escrow], marketStatus, 10);

		expect(forfeits.map((f) => [f.marketIndex, f.orderIndex])).to.deep.equal([
			[1, 1],
			[1, 3],
		]);
		expect(findRevenueShareForfeits([escrow], marketStatus, 1)).to.have.length(
			1
		);
	});
});
