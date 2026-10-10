import * as _ from 'lodash';
import * as fs from 'fs';
import * as path from 'path';
import { assert } from 'chai';
import {
	BN,
	MMOraclePriceData,
	PositionDirection,
	calculateAmmReservesAfterSwap,
	calculateNewAmm,
	getSwapDirection,
} from '../../src';
import { mockPerpMarkets } from '../dlob/helpers';

// Shared with the program's repeg `parity_fixtures` test: both implementations run the optimal peg,
// the budget and `adjust_amm` on each row and assert the program's curve and cost.
describe('adjust_amm parity with the program fixtures', () => {
	const rows = fs
		.readFileSync(path.join(__dirname, 'fixtures', 'adjust_amm.csv'), 'utf8')
		.trim()
		.split('\n')
		.slice(1)
		.map((line) => line.split(','));

	it('calculateNewAmm matches adjust_amm', () => {
		assert.isAbove(rows.length, 0);
		for (const [i, c] of rows.entries()) {
			const amm = _.cloneDeep(mockPerpMarkets[0].amm);
			amm.baseAssetReserve = new BN(c[0]);
			amm.quoteAssetReserve = new BN(c[1]);
			amm.sqrtK = new BN(c[2]);
			amm.pegMultiplier = new BN(c[3]);
			amm.baseAssetAmountWithAmm = new BN(c[4]);
			amm.minBaseAssetReserve = new BN(c[5]);
			amm.maxBaseAssetReserve = new BN(c[6]);
			amm.curveUpdateIntensity = Number(c[7]);
			amm.maxSpread = Number(c[8]);
			amm.totalFeeMinusDistributions = new BN(c[9]);
			const kUpdateGate = {
				minOrderSize: new BN(c[10]),
				marketConfig: Number(c[11]),
			};
			const [terminalQuoteAssetReserve] = calculateAmmReservesAfterSwap(
				amm,
				'base',
				amm.baseAssetAmountWithAmm.abs(),
				getSwapDirection(
					'base',
					amm.baseAssetAmountWithAmm.gt(new BN(0))
						? PositionDirection.SHORT
						: PositionDirection.LONG
				)
			);
			amm.terminalQuoteAssetReserve = terminalQuoteAssetReserve;
			const mmOraclePriceData = {
				price: new BN(c[12]),
				slot: new BN(0),
				confidence: new BN(1),
				hasSufficientNumberOfDataPoints: true,
				isMMOracleActive: true,
			} as MMOraclePriceData;

			const [cost, , , , checkLowerBound, curve] = calculateNewAmm(
				amm,
				mmOraclePriceData,
				kUpdateGate
			);
			const row = `row ${i + 1}`;
			assert.equal(curve.pegMultiplier.toString(), c[13], `${row} peg`);
			assert.equal(curve.sqrtK.toString(), c[14], `${row} sqrtK`);
			assert.equal(curve.baseAssetReserve.toString(), c[15], `${row} base`);
			assert.equal(curve.quoteAssetReserve.toString(), c[16], `${row} quote`);
			assert.equal(cost.toString(), c[17], `${row} cost`);
			assert.equal(String(checkLowerBound), c[18], `${row} check`);
		}
	});
});
