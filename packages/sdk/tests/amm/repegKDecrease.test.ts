import {
	AMM,
	BN,
	BASE_PRECISION,
	MarketConfigFlag,
	MMOraclePriceData,
	PEG_PRECISION,
	PRICE_PRECISION,
	PositionDirection,
	calculateAmmReservesAfterSwap,
	calculateNewAmm,
	calculateUpdatedAMM,
	getSwapDirection,
} from '../../src';
import { mockPerpMarkets } from '../dlob/helpers';
import { assert } from '../../src/assert/assert';
import * as _ from 'lodash';

// Mirrors adjust_amm in programs/velocity/src/vlp/amm/math/repeg.rs. A budget-limited repeg
// may lower k by 0.1%. The k decrease earns, that gain widens the peg budget, and the returned
// cost includes it, so the update never costs more than the budget.
describe('budget-limited repeg with a k decrease', () => {
	// Users net long 100 base on a 1000/1000 curve at peg 10, with 1000 units of fees. A 1% oracle
	// move up costs the AMM far more than that.
	function makeAmm(): AMM {
		const amm = _.cloneDeep(mockPerpMarkets[0].amm);
		amm.baseAssetReserve = new BN(1000).mul(BASE_PRECISION);
		amm.quoteAssetReserve = new BN(1000).mul(BASE_PRECISION);
		amm.sqrtK = new BN(1000).mul(BASE_PRECISION);
		amm.pegMultiplier = new BN(10).mul(PEG_PRECISION);
		amm.baseAssetAmountWithAmm = new BN(100).mul(BASE_PRECISION);
		amm.minBaseAssetReserve = new BN(500).mul(BASE_PRECISION);
		amm.maxBaseAssetReserve = new BN(2000).mul(BASE_PRECISION);
		amm.curveUpdateIntensity = 100;
		amm.maxSpread = 25000;
		amm.totalFeeMinusDistributions = new BN(1000);
		amm.netRevenueSinceLastFunding = new BN(0);
		const [terminalQuoteAssetReserve] = calculateAmmReservesAfterSwap(
			amm,
			'base',
			amm.baseAssetAmountWithAmm.abs(),
			getSwapDirection('base', PositionDirection.SHORT)
		);
		amm.terminalQuoteAssetReserve = terminalQuoteAssetReserve;
		return amm;
	}

	const mmOraclePriceData = {
		price: new BN(101).mul(PRICE_PRECISION).divn(10),
		slot: new BN(0),
		confidence: new BN(1),
		hasSufficientNumberOfDataPoints: true,
		isMMOracleActive: true,
	} as MMOraclePriceData;

	it('books the k decrease gain so the cost stays within the budget', () => {
		const amm = makeAmm();

		const [cost, pKNumer, pKDenom, newPeg] = calculateNewAmm(
			amm,
			mmOraclePriceData
		);
		assert(pKNumer.lt(pKDenom));
		// Row 1 of the shared adjust_amm.csv fixture: repeg cost on the lowered curve plus the k
		// adjustment, as the program computes it.
		assert(cost.eq(new BN(997)));
		assert(cost.lte(amm.totalFeeMinusDistributions));
		assert(newPeg.eq(new BN(10000921)));

		const updated = calculateUpdatedAMM(amm, mmOraclePriceData);
		assert(updated.sqrtK.lt(amm.sqrtK));
		assert(updated.pegMultiplier.eq(newPeg));
		assert(updated.totalFeeMinusDistributions.eq(new BN(3)));
	});

	it('repegs on the plain budget when the market disables the k decrease', () => {
		const amm = makeAmm();
		const kUpdateGate = {
			minOrderSize: new BN(0),
			marketConfig: MarketConfigFlag.DISABLE_FORMULAIC_K_UPDATE,
		};

		const [cost, pKNumer, pKDenom, newPeg] = calculateNewAmm(
			amm,
			mmOraclePriceData,
			kUpdateGate
		);
		assert(pKNumer.eq(new BN(1)) && pKDenom.eq(new BN(1)));
		assert(cost.lte(amm.totalFeeMinusDistributions));
		assert(newPeg.eq(new BN(10000010)));
		assert(newPeg.lt(new BN(10000921)));
	});

	it('does not lower k below curve update intensity 100', () => {
		const amm = makeAmm();
		amm.curveUpdateIntensity = 99;
		const [, pKNumer] = calculateNewAmm(amm, mmOraclePriceData);
		assert(pKNumer.eq(new BN(1)));
	});

	it('does not lower k when the minimum order size reaches sqrtK', () => {
		const amm = makeAmm();
		const [, pKNumer] = calculateNewAmm(amm, mmOraclePriceData, {
			minOrderSize: amm.sqrtK,
			marketConfig: 0,
		});
		assert(pKNumer.eq(new BN(1)));
	});
});
