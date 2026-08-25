import {
	BN,
	ZERO,
	depositAmountAndSharesForIfStake,
	getInsuranceFundNav,
	getInsuranceFundRevenueReceivableTokenAmount,
	timeRemainingUntilUpdate,
	ONE,
	SpotMarketAccount,
	SPOT_MARKET_BALANCE_PRECISION,
	SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION,
	QUOTE_PRECISION,
} from '../../src';
// import { mockPerpMarkets } from '../dlob/helpers';

import { assert } from '../../src/assert/assert';

describe('Insurance Tests', () => {
	it('time remaining updates', () => {
		const now = new BN(1683576852);
		const lastUpdate = new BN(1683576000);
		const period = new BN(3600); //hourly

		let tr;
		// console.log(now.sub(lastUpdate).toString());

		tr = timeRemainingUntilUpdate(now, lastUpdate, period);
		// console.log(tr.toString());
		assert(tr.eq(new BN('2748')));

		tr = timeRemainingUntilUpdate(now, lastUpdate.sub(period), period);
		// console.log(tr.toString());
		assert(tr.eq(ZERO));

		const tooLateUpdate = lastUpdate.sub(period.div(new BN(3)).add(ONE));
		tr = timeRemainingUntilUpdate(
			tooLateUpdate.add(ONE),
			tooLateUpdate,
			period
		);
		// console.log(tr.toString());
		assert(tr.eq(new BN('4800')));

		tr = timeRemainingUntilUpdate(now, lastUpdate.add(ONE), period);
		// console.log(tr.toString());
		assert(tr.eq(new BN('2748')));

		tr = timeRemainingUntilUpdate(now, lastUpdate.sub(ONE), period);
		// console.log(tr.toString());
		assert(tr.eq(new BN('2748')));
	});

	it('deposit is priced to whole shares', () => {
		// share price 1_000_000 (one share against a 1_000_000 vault): a request worth 1.5
		// shares buys one share and is charged one share price, not the full request
		let priced = depositAmountAndSharesForIfStake(
			new BN(1_500_000),
			new BN(1),
			new BN(1_000_000)
		);
		assert(priced.nShares.eq(new BN(1)));
		assert(priced.amountToDeposit.eq(new BN(1_000_000)));

		// a request below the price of a single share buys nothing (the program reverts)
		priced = depositAmountAndSharesForIfStake(
			new BN(999_999),
			new BN(1),
			new BN(1_000_000)
		);
		assert(priced.nShares.eq(ZERO));
		assert(priced.amountToDeposit.eq(ZERO));

		// an empty fund mints 1:1, so the whole request is charged
		priced = depositAmountAndSharesForIfStake(new BN(100), ZERO, ZERO);
		assert(priced.nShares.eq(new BN(100)));
		assert(priced.amountToDeposit.eq(new BN(100)));

		// share price below one (post-rebase regime): every unit of the request is spendable
		priced = depositAmountAndSharesForIfStake(
			new BN(7),
			new BN(1000),
			new BN(100)
		);
		assert(priced.nShares.eq(new BN(70)));
		assert(priced.amountToDeposit.eq(new BN(7)));

		// awkward share price: the charge rounds up to the shares' exact value, never above
		// the request, and never leaves a full share price behind
		const totalIfShares = new BN(7);
		const vaultBalance = new BN(1_000_003);
		for (const amount of [142_858, 1_000_004, 2_000_005, 12_345_678]) {
			priced = depositAmountAndSharesForIfStake(
				new BN(amount),
				totalIfShares,
				vaultBalance
			);
			assert(priced.amountToDeposit.lte(new BN(amount)));
			assert(
				new BN(amount)
					.sub(priced.amountToDeposit)
					.mul(totalIfShares)
					.lt(vaultBalance)
			);
			// the minted shares are never worth more than what was charged for them
			assert(
				priced.nShares
					.mul(vaultBalance.add(priced.amountToDeposit))
					.lte(priced.amountToDeposit.mul(totalIfShares.add(priced.nShares)))
			);
		}
	});

	it('an empty vault with shares outstanding is rejected', () => {
		// the program validates this state with `InvalidIFSharesDetected` before pricing 1:1
		let threw = false;
		try {
			depositAmountAndSharesForIfStake(new BN(100), new BN(1), ZERO);
		} catch (e) {
			threw = true;
		}
		assert(threw);
	});

	it('includes booked revenue in insurance fund nav', () => {
		const spotMarket = {
			decimals: 6,
			insuranceFundRevenueReceivableScaled: new BN(50)
				.mul(SPOT_MARKET_BALANCE_PRECISION)
				.div(QUOTE_PRECISION),
			cumulativeDepositInterest: SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION,
		} as SpotMarketAccount;

		assert(getInsuranceFundNav(spotMarket, new BN(1_000)).eq(new BN(1_050)));
	});

	it('grows the receivable with deposit interest', () => {
		// The claim is a scaled balance, so its token value rises with the
		// deposit index while the transfer cannot complete.
		const spotMarket = {
			decimals: 6,
			insuranceFundRevenueReceivableScaled: new BN(50)
				.mul(SPOT_MARKET_BALANCE_PRECISION)
				.div(QUOTE_PRECISION),
			cumulativeDepositInterest:
				SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION.muln(11).divn(10),
		} as SpotMarketAccount;

		assert(
			getInsuranceFundRevenueReceivableTokenAmount(spotMarket).eq(new BN(55))
		);
	});
});
