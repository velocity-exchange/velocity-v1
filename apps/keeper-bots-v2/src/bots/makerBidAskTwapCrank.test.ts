import { expect } from 'chai';
import { BN, QUOTE_PRECISION } from '@velocity-exchange/sdk';
import { ifStakeTopUp } from './makerBidAskTwapCrank';

const MIN_STAKE = new BN(1000).mul(QUOTE_PRECISION);

/** The program's valuation of a fresh deposit: shares rounded down, then value rounded down. */
function stakedValueAfterDeposit(vault: BN, totalShares: BN, deposit: BN): BN {
	const shares = deposit.mul(totalShares).div(vault);
	return shares.mul(vault.add(deposit)).div(totalShares.add(shares));
}

describe('ifStakeTopUp', () => {
	// The sandbox insurance fund at the time a 1000 quote stake read one share short.
	const vault = new BN('63668969033');
	const totalShares = new BN('54609713168');

	it('reaches the minimum from no stake', () => {
		const deposit = ifStakeTopUp(MIN_STAKE, new BN(0));

		expect(
			stakedValueAfterDeposit(vault, totalShares, deposit).gte(MIN_STAKE)
		).to.equal(true);
	});

	it('stakes more than one share when the stake reads one unit short', () => {
		const deposit = ifStakeTopUp(MIN_STAKE, MIN_STAKE.subn(1));

		expect(deposit.mul(totalShares).div(vault).gtn(0)).to.equal(true);
	});
});
