import { expect } from 'chai';
import { BN } from '@coral-xyz/anchor';
import { calculateEstimatedPerpEntryPrice } from '../../src/math/trade';
import { MMOraclePriceData } from '../../src/oracles/types';
import { PositionDirection } from '../../src/types';
import { L2OrderBook } from '../../src/orderBookLevels';
import {
	BASE_PRECISION,
	PEG_PRECISION,
	PRICE_PRECISION,
	ZERO,
} from '../../src/constants/numericConstants';
import { PerpMarketAccount } from '../../src/types';
import { mockPerpMarkets } from '../fixtures/mockAccounts';

/**
 * The walk consumes published book levels, and a published level is the sum of
 * several orders, so its size does not divide evenly at its price.
 *
 * In quote terms both conversions round down. A level can therefore keep a
 * residual worth zero quote, which fills no base and leaves the level as it
 * was. The walk has to move past such a level. Before it did, it repeated the
 * level forever and froze whichever process asked for the estimate.
 */
describe('perp entry price walk', () => {
	const ORACLE = new BN(3).mul(PRICE_PRECISION);

	/**
	 * A market whose curve quotes 3 and holds real depth on both sides, so the
	 * vAMM leg of the walk runs rather than dividing by an empty reserve. The
	 * shared mock's AMM has none, which is fine for the callers that only read
	 * its layout.
	 */
	const RESERVE = new BN(1_000).mul(BASE_PRECISION);
	function marketAtThree(): PerpMarketAccount {
		const base = mockPerpMarkets[0];
		return {
			...base,
			amm: {
				...base.amm,
				baseAssetReserve: RESERVE,
				quoteAssetReserve: RESERVE,
				askBaseAssetReserve: RESERVE,
				askQuoteAssetReserve: RESERVE,
				bidBaseAssetReserve: RESERVE,
				bidQuoteAssetReserve: RESERVE,
				sqrtK: RESERVE,
				pegMultiplier: new BN(3).mul(PEG_PRECISION),
				minBaseAssetReserve: RESERVE.divn(2),
				maxBaseAssetReserve: RESERVE.muln(2),
			},
		};
	}

	const mmOraclePriceData: MMOraclePriceData = {
		price: ORACLE,
		slot: new BN(0),
		confidence: new BN(1),
		hasSufficientNumberOfDataPoints: true,
		isMMOracleActive: true,
	};

	/** One ask whose size does not divide evenly at its price. */
	const dustBook = (): L2OrderBook => ({
		asks: [
			{
				price: new BN(3).mul(PRICE_PRECISION),
				size: BASE_PRECISION.add(new BN(1)),
				sources: {},
			},
		],
		bids: [],
	});

	function walk(assetType: 'base' | 'quote', amount: BN) {
		return calculateEstimatedPerpEntryPrice(
			assetType,
			amount,
			PositionDirection.LONG,
			marketAtThree(),
			mmOraclePriceData,
			dustBook(),
			0
		);
	}

	it('terminates in quote terms over a level that cannot divide evenly', () => {
		const result = walk('quote', new BN(10).mul(PRICE_PRECISION));
		expect(result.quoteFilled.gt(ZERO)).to.equal(true);
		expect(result.baseFilled.gt(ZERO)).to.equal(true);
	});

	it('still takes the whole level in base terms', () => {
		const result = walk('base', BASE_PRECISION.add(new BN(1)));
		expect(result.baseFilled.toString()).to.equal(
			BASE_PRECISION.add(new BN(1)).toString()
		);
	});
});
