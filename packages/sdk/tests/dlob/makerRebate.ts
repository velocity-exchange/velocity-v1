import { expect } from 'chai';
import _ from 'lodash';
import { DLOB, MarketType, StateAccount } from '../../src';
import { mockPerpMarkets, mockSpotMarkets, mockStateAccount } from './helpers';

/** Rebates that differ per tier, so the selected tier is observable. */
const REBATE_NUMERATOR_BY_TIER = [1, 2, 3];

function makeStateAccount(promoFeeTier: number): StateAccount {
	const state = _.cloneDeep(mockStateAccount);
	const feeTiers = REBATE_NUMERATOR_BY_TIER.map((makerRebateNumerator) => ({
		...state.perpFeeStructure.feeTiers[0],
		makerRebateNumerator,
		makerRebateDenominator: 10_000,
	}));

	return {
		...state,
		promoFeeTier,
		perpFeeStructure: { ...state.perpFeeStructure, feeTiers },
	};
}

describe('DLOB.getMakerRebate', () => {
	const dlob = new DLOB();
	const perpMarket = _.cloneDeep(mockPerpMarkets[0]);
	const spotMarket = _.cloneDeep(mockSpotMarkets[0]);

	it('reads the entry tier while no promo is set', () => {
		const { makerRebateNumerator } = dlob.getMakerRebate(
			MarketType.PERP,
			makeStateAccount(0),
			perpMarket
		);

		expect(makerRebateNumerator).to.equal(REBATE_NUMERATOR_BY_TIER[0]);
	});

	// A promo floors every account's tier, so tier 0's rebate is one no maker
	// earns while it is set.
	it('reads the promo tier while one is set', () => {
		const { makerRebateNumerator } = dlob.getMakerRebate(
			MarketType.PERP,
			makeStateAccount(2),
			perpMarket
		);

		expect(makerRebateNumerator).to.equal(REBATE_NUMERATOR_BY_TIER[2]);
	});

	it('clamps a promo above the live tiers', () => {
		const { makerRebateNumerator } = dlob.getMakerRebate(
			MarketType.PERP,
			makeStateAccount(9),
			perpMarket
		);

		expect(makerRebateNumerator).to.equal(REBATE_NUMERATOR_BY_TIER[2]);
	});

	it('leaves spot on the entry tier', () => {
		const state = makeStateAccount(2);
		const { makerRebateNumerator, makerRebateDenominator } =
			dlob.getMakerRebate(MarketType.SPOT, state, spotMarket);

		expect(makerRebateNumerator).to.equal(
			state.spotFeeStructure.feeTiers[0].makerRebateNumerator
		);
		expect(makerRebateDenominator).to.equal(
			state.spotFeeStructure.feeTiers[0].makerRebateDenominator
		);
	});
});
