import { assert } from 'chai';
import {
	BN,
	BASE_PRECISION,
	PRICE_PRECISION,
	QUOTE_PRECISION,
	LIQUIDATION_PCT_PRECISION,
	calculateMaxPctToLiquidate,
	getLiquidationFee,
	millisFromStoredUnits,
	calculatePerpIfFee,
	calculateSpotIfFee,
	calculateUserProtectiveAssetPrice,
	calculateUserProtectiveLiabilityPrice,
} from '../../src';

describe('calculateMaxPctToLiquidate', () => {
	it('isolated position override returns 100% regardless of graduated schedule', () => {
		const pct = calculateMaxPctToLiquidate(
			new BN(0), // userLastActiveSlot
			new BN(0), // userLiquidationMarginFreed
			new BN(1_000_000).mul(QUOTE_PRECISION), // huge margin shortage
			new BN(0), // slot === lastActiveSlot, no time elapsed
			new BN(0), // initialPctToLiquidate
			millisFromStoredUnits(1000), // liquidationDuration
			true // isIsolatedPosition
		);

		assert.isTrue(pct.eq(LIQUIDATION_PCT_PRECISION));
	});

	it('computes slots elapsed unconditionally, even when no margin has been freed yet', () => {
		// userLiquidationMarginFreed === 0: a prior gate on this value would force
		// slotsElapsed to 0 and the whole schedule to be stuck at initialPctToLiquidate.
		const pct = calculateMaxPctToLiquidate(
			new BN(0), // userLastActiveSlot
			new BN(0), // userLiquidationMarginFreed
			new BN(1000).mul(QUOTE_PRECISION), // margin shortage (above the 50 QUOTE_PRECISION floor)
			new BN(100), // slot
			new BN(0), // initialPctToLiquidate
			millisFromStoredUnits(1000) // liquidationDuration
		);

		// slotsElapsed = 100, pctFreeable = 100 * 10000 / 1000 = 1000 (10%)
		assert.isTrue(pct.eq(new BN(1000)));
	});
});

describe('getLiquidationFee', () => {
	it('matches elapsed wall-clock time at 400ms and 200ms', () => {
		const baseFee = 20_000;
		const maxFee = 50_000;
		const baseline = getLiquidationFee(
			baseFee,
			maxFee,
			new BN(0),
			new BN(10_000),
			{}
		);
		// a clock fully rolled out to 200ms since slot 1
		const clock200 = {
			slotDurationTransitionSlots: [new BN(1), new BN(1), new BN(1), new BN(1)],
		};
		const fast = getLiquidationFee(
			baseFee,
			maxFee,
			new BN(1_000),
			new BN(1_000 + 20_000),
			clock200
		);

		assert.equal(baseline, 30_000);
		assert.equal(fast, baseline);
	});

	it('integrates an interval spanning a slot-duration transition piecewise', () => {
		// 350ms regime starts at slot 3_000; the interval covers 3000 slots at
		// 400ms + 3000 at 350ms = 2_250_000ms = 5625 whole 400ms periods
		const clock = {
			slotDurationTransitionSlots: [
				new BN(3_000),
				new BN(0),
				new BN(0),
				new BN(0),
			],
		};
		const fee = getLiquidationFee(0, 100_000, new BN(0), new BN(6_000), clock);
		assert.equal(fee, 5_625);
	});

	it('rejects a current slot before the last-active slot like the program', () => {
		assert.throws(
			() => getLiquidationFee(20_000, 50_000, new BN(101), new BN(100)),
			/currentSlot must not precede lastActiveUserSlot/
		);
	});
});

describe('calculatePerpIfFee', () => {
	// marginRatio 5%, liquidator fee 0.5%, quote oracle price != 1.0
	const marginRatio = 500;
	const liquidatorFee = 5000;
	const oraclePrice = new BN(100).mul(new BN(1_000_000));
	const quoteOraclePrice = new BN(1_020_000); // 1.02
	const userBaseAssetAmount = new BN(10).mul(BASE_PRECISION);
	const marginShortage = new BN(1).mul(QUOTE_PRECISION);

	it('returns the implied fee when it is below the combined-rate cap', () => {
		const fee = calculatePerpIfFee(
			marginShortage,
			userBaseAssetAmount,
			marginRatio,
			liquidatorFee,
			oraclePrice,
			quoteOraclePrice,
			50_000 // cap well above the implied fee
		);

		assert.equal(fee, 41_819);
	});

	it('clamps to the combined-rate cap when the implied fee exceeds it', () => {
		const fee = calculatePerpIfFee(
			marginShortage,
			userBaseAssetAmount,
			marginRatio,
			liquidatorFee,
			oraclePrice,
			quoteOraclePrice,
			20_000 // cap below the implied fee
		);

		assert.equal(fee, 20_000);
	});
});

describe('calculateSpotIfFee', () => {
	const assetWeight = 8000;
	const liabilityWeight = 12000;
	const assetLiquidationMultiplier = 1_000_000;
	const liabilityLiquidationMultiplier = 1_000_000;
	const liabilityDecimals = 6;
	const liabilityPrice = new BN(1_050_000); // non-1.0 price
	const tokenAmount = new BN(1000).mul(
		new BN(10).pow(new BN(liabilityDecimals))
	);
	const marginShortage = new BN(10).mul(QUOTE_PRECISION);

	it('returns the implied fee when it is below the combined-rate cap', () => {
		const fee = calculateSpotIfFee(
			marginShortage,
			tokenAmount,
			assetWeight,
			assetLiquidationMultiplier,
			liabilityWeight,
			liabilityLiquidationMultiplier,
			liabilityDecimals,
			liabilityPrice,
			400_000 // cap well above the implied fee
		);

		assert.equal(fee, 325_397);
	});

	it('clamps to the combined-rate cap when the implied fee exceeds it', () => {
		const fee = calculateSpotIfFee(
			marginShortage,
			tokenAmount,
			assetWeight,
			assetLiquidationMultiplier,
			liabilityWeight,
			liabilityLiquidationMultiplier,
			liabilityDecimals,
			liabilityPrice,
			100_000 // cap below the implied fee
		);

		assert.equal(fee, 100_000);
	});
});

describe('calculateUserProtectiveAssetPrice', () => {
	it('uses the 5min twap when the (stale) oracle price is below it', () => {
		const price = calculateUserProtectiveAssetPrice(
			new BN(90).mul(PRICE_PRECISION),
			new BN(0),
			new BN(100).mul(PRICE_PRECISION)
		);

		assert.isTrue(price.eq(new BN(100).mul(PRICE_PRECISION)));
	});

	it('uses the confidence-adjusted high when it exceeds oracle and twap', () => {
		const price = calculateUserProtectiveAssetPrice(
			new BN(100).mul(PRICE_PRECISION),
			new BN(5).mul(PRICE_PRECISION),
			new BN(95).mul(PRICE_PRECISION)
		);

		assert.isTrue(price.eq(new BN(105).mul(PRICE_PRECISION)));
	});

	it('never returns less than the raw oracle price', () => {
		const price = calculateUserProtectiveAssetPrice(
			new BN(110).mul(PRICE_PRECISION),
			new BN(0),
			new BN(100).mul(PRICE_PRECISION)
		);

		assert.isTrue(price.eq(new BN(110).mul(PRICE_PRECISION)));
	});
});

describe('calculateUserProtectiveLiabilityPrice', () => {
	it('uses the 5min twap when the (stale) oracle price is above it', () => {
		const price = calculateUserProtectiveLiabilityPrice(
			new BN(110).mul(PRICE_PRECISION),
			new BN(0),
			new BN(100).mul(PRICE_PRECISION)
		);

		assert.isTrue(price.eq(new BN(100).mul(PRICE_PRECISION)));
	});

	it('uses the confidence-adjusted low when it is below oracle and twap', () => {
		const price = calculateUserProtectiveLiabilityPrice(
			new BN(100).mul(PRICE_PRECISION),
			new BN(5).mul(PRICE_PRECISION),
			new BN(105).mul(PRICE_PRECISION)
		);

		assert.isTrue(price.eq(new BN(95).mul(PRICE_PRECISION)));
	});

	it('never returns more than the raw oracle price and floors at 1', () => {
		const price = calculateUserProtectiveLiabilityPrice(
			new BN(90).mul(PRICE_PRECISION),
			new BN(0),
			new BN(100).mul(PRICE_PRECISION)
		);

		assert.isTrue(price.eq(new BN(90).mul(PRICE_PRECISION)));

		const floored = calculateUserProtectiveLiabilityPrice(
			new BN(100).mul(PRICE_PRECISION),
			new BN(200).mul(PRICE_PRECISION),
			new BN(100).mul(PRICE_PRECISION)
		);

		assert.isTrue(floored.eq(new BN(1)));
	});
});
